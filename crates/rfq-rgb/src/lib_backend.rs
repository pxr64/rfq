use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;

use async_trait::async_trait;
use rfq_btc::{BitcoinClient, ElectrumClient};
use rfq_consignment::MinedChecker;
use rfq_types::{AssetId, Outpoint, RgbInventoryUtxo, SwapTransfer};

use amplify::Wrapper as _;
use base64::Engine as _;
use bpstd::psbt::Psbt;
use bpstd::signers::TestnetRefSigner;
use bpstd::{HardenedIndex, Keychain, Network, Sats, XprivAccount, XpubDerivable};
use bpwallet::fs::FsTextStore;
use bpwallet::hot::{SecureIo, Seed, SeedType};
use bpwallet::{Bip43, Wallet};
use psrgbt::PsbtConstructor;
use rgb::containers::{ConsignmentExt, FileContent, Kit, Transfer};
use rgb::contract::FilterIncludeAll;
use rgb::invoice::{
    Beneficiary, InvoiceState, Pay2Vout, Precision, RgbInvoice, RgbInvoiceBuilder, XChainNet,
};
use rgb::persistence::fs::FsBinStore;
use rgb::persistence::{StashReadProvider, Stock};
use rgb::resolvers::{AnyResolver, ContractIssueResolver};
use rgb::stl::{AssetSpec, ContractTerms, RicardianContract};
use rgb::validation::{ResolveWitness, ValidationConfig, Validity};
use rgb::{
    Amount, ChainNet, ContractId, GenesisSeal, GraphSeal, Identity, OutputSeal, RgbDescr,
    RgbKeychain, RgbWallet, StateType, TapretKey, TransferParams,
};

use crate::swap;

use crate::{ConsignmentInfo, FinalizedSwap, MakerInvoiceParts, RgbBackend, RgbError, TxOut};

/// The Non-Inflatable Asset (NIA) schema kit, embedded so issuance has no runtime
/// file dependency. The schema is network-agnostic (a fixed schema id); only the
/// genesis seal's outpoint is network-specific, so the same binary issues on
/// regtest/signet/mainnet. Source: rgb-schemata's NonInflatableAsset.
const NIA_SCHEMA_KIT: &[u8] = include_bytes!("../schemas/NonInflatableAsset.rgb");

/// Library-backed `RgbBackend` implementation. Talks directly to `rgb-api` /
/// `rgb-ops` (the same libraries `rgb-cmd` is built on) — no subprocess.
///
/// Stashes follow the rgb-cmd convention: `<data_dir>/<network>/...`. So
/// existing stashes created by `make rgb-wallets-init` + `rgb_<role> create
/// --tapret-key-only ...` are reusable without re-import.
pub struct LibRgbBackend {
    data_dir: PathBuf,
    wallet_name: String,
    network: String,
    // Wired into the resolver below; consumed by `validate_incoming_consignment`
    // + the swap-PSBT methods (next #13 session).
    #[allow(dead_code)]
    electrum_url: String,
    // Cold/hot signer split: `load_wallet()` is xpub-only (can't sign); the
    // swap-PSBT methods load this encrypted account file to sign the maker's
    // own inputs. See docs/swap-psbt-design.md U6.
    #[allow(dead_code)]
    signer_account_file: PathBuf,
    #[allow(dead_code)]
    signer_password: String,
    // Serializes ALL stock/wallet file access. The `FsBinStore`/`FsTextStore`
    // back the stock + bp-wallet as files, and `Stock::load(_, true)` autosaves on
    // drop — so two concurrent operations (the 5s chain-observer loop vs a request
    // handler) interleave a read with a half-written file → "unexpected end of
    // file". Every method that loads the stock/wallet holds this first (declared
    // before the stock local, so it releases AFTER the autosave-on-drop). The
    // guarded bodies are synchronous (no `.await` while held), so the futures stay
    // `Send`. See `guard()`.
    stock_lock: std::sync::Mutex<()>,
}

impl LibRgbBackend {
    pub fn new(
        data_dir: PathBuf,
        wallet_name: String,
        network: String,
        electrum_url: String,
        signer_account_file: PathBuf,
        signer_password: String,
    ) -> Self {
        Self {
            data_dir,
            wallet_name,
            network,
            electrum_url,
            signer_account_file,
            signer_password,
            stock_lock: std::sync::Mutex::new(()),
        }
    }

    /// Acquire the stock-access lock, recovering from a poisoned mutex (a prior
    /// panic mid-operation shouldn't wedge the maker — the next load re-reads the
    /// file from scratch anyway). Bind this FIRST in any method that loads the
    /// stock/wallet so it outlives the autosave-on-drop.
    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.stock_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Create a fresh taproot (tapret) RGB wallet + an empty Stock on disk,
    /// Rust-native (no `rgb-cmd`/docker) — the operator path that replaces
    /// `rgb create --tapret-key-only`. Generates a new seed, writes the encrypted
    /// signing account to `signer_account_file`, derives a BIP-86 taproot
    /// descriptor on the RGB keychains `/<0;1;10>/*`, and persists the wallet
    /// (`FsTextStore`) + Stock (`FsBinStore`) at the same paths `load_wallet` /
    /// `load_stock` read from. Errors if a wallet already exists there.
    pub fn create_wallet(&self) -> Result<(), RgbError> {
        let network = self.parse_network()?;
        if self.wallet_path().exists() {
            return Err(RgbError::StashLoad(format!(
                "wallet already exists at {}",
                self.wallet_path().display()
            )));
        }

        // Fresh seed → BIP-86 taproot account. A testnet xpub (tpub) for any
        // non-mainnet network (signet/testnet/regtest); mainnet gets an xpub.
        let seed = Seed::random(SeedType::Bit128);
        let account = seed.derive(
            Bip43::Bip86,
            !matches!(network, Network::Mainnet),
            HardenedIndex::hardened(0),
        );

        // Persist the encrypted signing account — the hot key the swap signs with.
        if let Some(parent) = self.signer_account_file.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| RgbError::StashLoad(format!("create account dir: {e}")))?;
        }
        account
            .write(&self.signer_account_file, &self.signer_password)
            .map_err(|e| RgbError::StashLoad(format!("write account file: {e}")))?;

        // Taproot descriptor on the RGB keychains (0=receive, 1=change,
        // 10=tapret anchors) → `RgbDescr::TapretKey`.
        let descriptor = format!("{}/<0;1;10>/*", account.to_xpub_account());
        let xpub = XpubDerivable::from_str(&descriptor)
            .map_err(|e| RgbError::StashLoad(format!("parse descriptor: {e}")))?;
        let rgb_descr: RgbDescr = TapretKey::from(xpub).into();

        // Create + persist the bp-wallet at the same path `load_wallet` reads.
        std::fs::create_dir_all(self.wallet_path())
            .map_err(|e| RgbError::StashLoad(format!("create wallet dir: {e}")))?;
        let mut wallet: Wallet<XpubDerivable, RgbDescr> = Wallet::new_layer1(rgb_descr, network);
        let provider = FsTextStore::new(self.wallet_path())
            .map_err(|e| RgbError::StashLoad(format!("wallet provider: {e}")))?;
        wallet
            .make_persistent(provider, true)
            .map_err(|e| RgbError::StashLoad(format!("wallet persist: {e}")))?;
        wallet.set_name(self.wallet_name.clone());
        wallet
            .store()
            .map_err(|e| RgbError::StashLoad(format!("wallet store: {e}")))?;

        // Create + persist a fresh empty Stock at the same path `load_stock` reads
        // (`make_persistent(_, true)` autosaves on drop).
        std::fs::create_dir_all(self.stock_path())
            .map_err(|e| RgbError::StashLoad(format!("create stock dir: {e}")))?;
        let stock_provider = FsBinStore::new(self.stock_path())
            .map_err(|e| RgbError::StashLoad(format!("stock provider: {e}")))?;
        let mut stock = Stock::in_memory();
        stock
            .make_persistent(stock_provider, true)
            .map_err(|e| RgbError::StashLoad(format!("stock persist: {e}")))?;
        drop(stock);

        Ok(())
    }

    /// Derive a receive address for the operator to fund manually from a faucet.
    /// `rgb_keychain` → the tapret RGB seal-anchor keychain (10); otherwise the
    /// external BTC keychain (0). Read-only: does not advance the wallet index.
    pub fn funding_address(&self, rgb_keychain: bool) -> Result<String, RgbError> {
        let mut wallet = self.load_wallet()?;
        let addr = if rgb_keychain {
            wallet.wallet_mut().next_address(RgbKeychain::Tapret, false)
        } else {
            wallet.wallet_mut().next_address(Keychain::OUTER, false)
        };
        Ok(addr.to_string())
    }

    /// Whether a wallet already exists on disk for this (data_dir, network,
    /// name) — lets callers (e.g. `colorex maker init`) skip `create_wallet`
    /// without clobbering an existing wallet.
    pub fn wallet_exists(&self) -> bool {
        self.wallet_path().exists()
    }

    /// Issue a Non-Inflatable Asset (NIA) RGB contract — the production "mint a
    /// token" path, Rust-native (no rgb-cmd). Imports the embedded NIA schema,
    /// builds the genesis (spec ticker/name/details/precision + total supply, all
    /// allocated to the issuer's `genesis_outpoint` seal), and persists it into
    /// the issuer's Stock. Returns the new contract id. `genesis_outpoint` must be
    /// a real unspent UTXO the issuer owns on this network — the first
    /// distribution transfer spends it. `issuer` is a label embedded in genesis
    /// (e.g. `ssi:anonymous`); no signing happens at issuance.
    pub fn issue_contract(
        &self,
        ticker: &str,
        name: &str,
        details: Option<&str>,
        precision: u8,
        supply: u64,
        genesis_outpoint: &str,
        issuer: &str,
    ) -> Result<String, RgbError> {
        let mut stock = self.load_stock()?;

        // Import the (network-agnostic) NIA schema kit.
        let kit = Kit::load(&mut &NIA_SCHEMA_KIT[..])
            .map_err(|e| RgbError::StashLoad(format!("load NIA kit: {e}")))?
            .validate()
            .map_err(|e| RgbError::StashLoad(format!("validate NIA kit: {e:?}")))?;
        let schema_id = kit
            .schemata
            .iter()
            .next()
            .ok_or_else(|| RgbError::StashLoad("NIA kit carries no schema".to_owned()))?
            .schema_id();
        stock
            .import_kit(kit)
            .map_err(|e| RgbError::StashLoad(format!("import kit: {e}")))?;

        // Build the genesis state.
        let spec = AssetSpec::with(
            ticker,
            name,
            Precision::try_from(precision)
                .map_err(|_| RgbError::TransferBuild(format!("bad precision {precision}")))?,
            details,
        )
        .map_err(|e| RgbError::TransferBuild(format!("asset spec: {e}")))?;
        let terms = ContractTerms {
            text: RicardianContract::default(),
            media: None,
        };
        let os = OutputSeal::from_str(genesis_outpoint).map_err(|e| {
            RgbError::TransferBuild(format!("bad genesis outpoint {genesis_outpoint}: {e}"))
        })?;
        let seal = GenesisSeal::new_random(os.txid, os.vout);

        let issuer_id = Identity::from_str(issuer)
            .map_err(|e| RgbError::TransferBuild(format!("bad issuer identity {issuer}: {e}")))?;
        let chain_net = chain_net_for(self.parse_network()?);
        let contract = stock
            .contract_builder(issuer_id, schema_id, chain_net)
            .map_err(|e| RgbError::TransferBuild(format!("contract builder: {e}")))?
            .add_global_state("spec", spec)
            .map_err(|e| RgbError::TransferBuild(format!("spec state: {e}")))?
            .add_global_state("terms", terms)
            .map_err(|e| RgbError::TransferBuild(format!("terms state: {e}")))?
            .add_global_state("issuedSupply", Amount::from(supply))
            .map_err(|e| RgbError::TransferBuild(format!("supply state: {e}")))?
            .add_fungible_state("assetOwner", seal, Amount::from(supply))
            .map_err(|e| RgbError::TransferBuild(format!("owner state: {e}")))?
            .issue_contract()
            .map_err(|e| RgbError::TransferBuild(format!("issue contract: {e}")))?;

        let id = contract.contract_id();
        stock
            .import_contract(contract, &ContractIssueResolver)
            .map_err(|e| RgbError::StashLoad(format!("import contract: {e}")))?;
        drop(stock); // autosave-on-drop persists the new contract
        Ok(id.to_string())
    }

    /// List issued contracts in the stock as `<id>  issuer=<id> net=<chain>` —
    /// for verifying `issue_contract` and discovering the contract id to put in
    /// the maker/taker config.
    pub fn list_contracts(&self) -> Result<Vec<String>, RgbError> {
        let stock = self.load_stock()?;
        let lines: Vec<String> = stock
            .contracts()
            .map_err(|e| RgbError::StashLoad(format!("list contracts: {e}")))?
            .map(|info| {
                format!(
                    "{}  issuer={} net={:?}",
                    info.id, info.issuer, info.chain_net
                )
            })
            .collect();
        Ok(lines)
    }

    /// Read the contract's ticker + precision (decimal places) from its `spec`
    /// global state — for human-readable inventory display (`COLX 1.00`).
    pub fn contract_spec(&self, asset: &AssetId) -> Result<(String, u8), RgbError> {
        let _guard = self.guard();
        let stock = self.load_stock()?;
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;
        let contract = stock
            .contract_data(contract_id)
            .map_err(|e| RgbError::ContractNotFound(format!("contract not found: {e}")))?;
        let spec_val = contract
            .global("spec")
            .next()
            .ok_or_else(|| RgbError::ContractNotFound("contract has no spec state".to_owned()))?;
        let spec = AssetSpec::from_strict_val_unchecked(&spec_val);
        Ok((spec.ticker().to_owned(), spec.precision.decimals()))
    }

    /// Pick a funded keychain-10 (tapret) UTXO to bind a contract genesis to,
    /// as `txid:vout`. The issuer wallet must be funded + synced first
    /// (`wallet sync`); the first distribution transfer spends this UTXO.
    pub fn pick_genesis_seal(&self) -> Result<String, RgbError> {
        let wallet = self.load_wallet()?;
        let seal = wallet
            .wallet()
            .utxos()
            .find(|u| RgbKeychain::contains_rgb(u.terminal.keychain))
            .map(|u| format!("{}:{}", u.outpoint.txid, u.outpoint.vout.into_u32()));
        seal.ok_or_else(|| {
            RgbError::TransferBuild(
                "no funded keychain-10 UTXO to bind genesis; fund + sync the issuer wallet first"
                    .to_owned(),
            )
        })
    }

    /// Stock lives at `<data_dir>/<network>/` — same layout rgb-cmd's
    /// `GeneralOpts::base_dir()` resolves to, so existing stashes are reusable.
    fn stock_path(&self) -> PathBuf {
        self.data_dir.join(&self.network)
    }

    /// Wallet sits one level below the stock dir, keyed by wallet name —
    /// mirrors rgb-cmd's `GeneralOpts::wallet_dir(name)`.
    fn wallet_path(&self) -> PathBuf {
        self.stock_path().join(&self.wallet_name)
    }

    fn load_stock(&self) -> Result<Stock, RgbError> {
        let provider =
            FsBinStore::new(self.stock_path()).map_err(|e| RgbError::StashLoad(e.to_string()))?;
        Stock::load(provider, true).map_err(|e| RgbError::StashLoad(e.to_string()))
    }

    /// Validate an incoming consignment and absorb it into *this* wallet's
    /// stash. The receiver-side counterpart of [`Self::validate_incoming_consignment`]
    /// (which is maker/sender oriented and also extracts the sender's input
    /// outpoints): a taker calls this to record RGB it just received — tokens
    /// bought (buy: the maker→taker transfer) or change from a sell (the
    /// maker-emitted transfer to the taker's change seal). It only needs the
    /// transfer accepted, so it skips `extract_input_outpoints` entirely.
    ///
    /// MUST be called after the swap tx confirms. Two phases: (1) validate +
    /// `accept_transfer` using a resolver seeded with the consignment's txes so
    /// the full witness graph is available; (2) `update_witnesses` with a fresh
    /// chain-only resolver to re-resolve each absorbed witness against electrum.
    /// Phase 2 is essential: the phase-1 resolver forces every consignment tx to
    /// `WitnessOrd::Tentative` (`AnyResolver` short-circuits `add_consignment_txes`,
    /// rgb-ops `indexers/any.rs`), and a Tentative allocation shows in
    /// `list_inventory_utxos` but is unspendable by `wallet.pay` and is dropped
    /// when contract state is recomputed on the next `sync_wallet`. Re-resolving
    /// promotes the mined swap tx to `WitnessOrd::Mined`, making the allocation
    /// spendable and durable. `Stock::load(_, true)` autosaves on drop.
    pub async fn accept_incoming_transfer(
        &self,
        consignment_base64: &str,
        expected_contract_id: ContractId,
    ) -> Result<(), RgbError> {
        let _guard = self.guard();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(consignment_base64.trim())
            .map_err(|e| {
                RgbError::TransferBuild(format!("consignment is not valid base64: {e}"))
            })?;

        let consignment = Transfer::load(bytes.as_slice())
            .map_err(|e| RgbError::TransferBuild(format!("consignment decode: {e}")))?;

        if consignment.contract_id() != expected_contract_id {
            return Err(RgbError::TransferBuild(format!(
                "consignment contract {} does not match expected {}",
                consignment.contract_id(),
                expected_contract_id
            )));
        }

        let mut stock = self.load_stock()?;

        // A recipient validates an NIA consignment against its stock's "trusted
        // typesystem" (below), so the NIA schema must already be in the stock.
        // `issue_contract` imports it for the issuer; a *separate* recipient
        // (maker/taker) has an empty stock and would hit `TypeSystemMismatch`, so
        // import the same embedded kit here. Idempotent on re-accept.
        let kit = Kit::load(&mut &NIA_SCHEMA_KIT[..])
            .map_err(|e| RgbError::StashLoad(format!("load NIA kit: {e}")))?
            .validate()
            .map_err(|e| RgbError::StashLoad(format!("validate NIA kit: {e:?}")))?;
        stock
            .import_kit(kit)
            .map_err(|e| RgbError::StashLoad(format!("import NIA kit: {e}")))?;

        // Validation needs the full witness graph, so seed the resolver with the
        // consignment's own txes. Caveat: `AnyResolver` then short-circuits every
        // such txid to `WitnessOrd::Tentative` (rgb-ops `indexers/any.rs`) — a
        // Tentative allocation is unspendable by `wallet.pay` and gets dropped
        // when contract state is recomputed on the next `sync_wallet`. Step 2
        // (`update_witnesses`) fixes that.
        let mut resolver = self.resolver()?;
        resolver.add_consignment_txes(&consignment);

        let validation_config = ValidationConfig {
            chain_net: chain_net_for(self.parse_network()?),
            trusted_typesystem: stock
                .as_stash_provider()
                .type_system()
                .map_err(|e| RgbError::StashLoad(format!("type system: {e}")))?
                .clone(),
            ..Default::default()
        };

        let validated = consignment
            .validate(&resolver, &validation_config)
            .map_err(|e| RgbError::TransferBuild(format!("consignment validation: {e:?}")))?;

        if validated.validation_status().validity() != Validity::Valid {
            return Err(RgbError::TransferBuild(format!(
                "consignment invalid: {}",
                validated.validation_status()
            )));
        }

        stock
            .accept_transfer(validated, &resolver)
            .map_err(|e| RgbError::StashLoad(format!("accept_transfer: {e}")))?;

        // Step 2: promote the just-absorbed witnesses from the forced Tentative
        // to their real chain ord. A FRESH chain-only resolver (no consignment
        // txes) re-resolves each witness against electrum, so the mined swap tx
        // becomes `WitnessOrd::Mined` — making the allocation spendable and
        // durable across re-syncs. Witnesses electrum can't resolve land in
        // `UpdateRes.failed` without aborting, so a not-yet-deep ancestor doesn't
        // block promotion of the rest. `after_height = 0` re-checks every
        // non-Ignored witness.
        let chain_resolver = self.resolver()?;
        stock
            .update_witnesses(chain_resolver, 0, vec![])
            .map_err(|e| RgbError::StashLoad(format!("update_witnesses: {e}")))?;

        Ok(())
    }

    /// SECURITY (buy-side gate): the taker calls this on the maker-delivered consignment
    /// BEFORE it signs/pays. A malicious maker could otherwise fabricate its inventory
    /// ancestry (RGB whose history was never mined on-chain) and take the taker's BTC for
    /// nonexistent RGB. Unlike the sell-side gate, the consignment's TERMINAL witness is
    /// the swap tx itself — not yet broadcast at sign time, so legitimately unmined. We
    /// therefore:
    ///   (a) graph-validate with a seeded resolver (so the unmined swap tx is available as
    ///       `Tentative` and the graph/schema/commitment checks pass), then
    ///   (b) require every OTHER witness (the maker's inventory ancestry) to be `Mined`,
    ///       resolved against a chain-only resolver. `expected_witness_txid` (the swap tx)
    ///       is the one hop allowed to be unmined.
    /// Reject on any unmined ancestry witness, or a graph-invalid consignment.
    pub async fn validate_buy_consignment(
        &self,
        consignment_base64: &str,
        expected_contract_id: ContractId,
        expected_witness_txid: &str,
    ) -> Result<(), RgbError> {
        let _guard = self.guard();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(consignment_base64.trim())
            .map_err(|e| {
                RgbError::TransferBuild(format!("consignment is not valid base64: {e}"))
            })?;
        let consignment = Transfer::load(bytes.as_slice())
            .map_err(|e| RgbError::TransferBuild(format!("consignment decode: {e}")))?;
        if consignment.contract_id() != expected_contract_id {
            return Err(RgbError::TransferBuild(format!(
                "consignment contract {} does not match expected {}",
                consignment.contract_id(),
                expected_contract_id
            )));
        }

        // (a) Graph validation. Seed the consignment txes so the not-yet-broadcast swap tx
        // (the terminal hop) resolves as `Tentative` and the graph checks pass. No
        // `safe_height` here precisely because that terminal is legitimately unmined; the
        // ancestry mining requirement is enforced explicitly in (b).
        let stock = self.load_stock()?;
        let mut resolver = self.resolver()?;
        resolver.add_consignment_txes(&consignment);
        let validation_config = ValidationConfig {
            chain_net: chain_net_for(self.parse_network()?),
            trusted_typesystem: stock
                .as_stash_provider()
                .type_system()
                .map_err(|e| RgbError::StashLoad(format!("type system: {e}")))?
                .clone(),
            ..Default::default()
        };
        let validated = consignment
            .validate(&resolver, &validation_config)
            .map_err(|e| RgbError::TransferBuild(format!("buy consignment validation: {e:?}")))?;
        if validated.validation_status().validity() != Validity::Valid {
            return Err(RgbError::TransferBuild(format!(
                "buy consignment invalid: {}",
                validated.validation_status()
            )));
        }

        // (b) Mined-ancestry gate via the shared `MinedChecker`: every witness the validator
        // walked must be mined on-chain, EXCEPT the expected swap tx (the not-yet-broadcast
        // terminal hop the taker is about to sign/broadcast).
        let txids: Vec<String> = validated
            .validation_status()
            .tx_ord_map
            .keys()
            .map(|t| t.to_string())
            .collect();
        let exempt = HashSet::from([expected_witness_txid.to_owned()]);
        let verdict = MinedChecker::new(vec![self.electrum_url.clone()], self.min_confs())
            .check(&txids, &exempt)
            .map_err(|e| RgbError::TransferBuild(format!("mined-ancestry check: {e}")))?;
        if !verdict.all_mined {
            return Err(RgbError::TransferBuild(format!(
                "buy consignment ancestry not mined on-chain (refusing to sign): {:?}",
                verdict.unmined
            )));
        }
        Ok(())
    }

    /// Load the full RGB wallet: a `bp-wallet` `Wallet<XpubDerivable, RgbDescr>`
    /// wrapped with the maker's `Stock`. The wallet descriptor must already
    /// exist on disk (e.g. via `make rgb-wallets-init` + `rgb create --tapret-key-only`).
    fn load_wallet(&self) -> Result<RgbWallet<Wallet<XpubDerivable, RgbDescr>>, RgbError> {
        let stock = self.load_stock()?;
        let provider = FsTextStore::new(self.wallet_path())
            .map_err(|e| RgbError::StashLoad(format!("wallet provider: {e}")))?;
        let wallet: Wallet<XpubDerivable, RgbDescr> = Wallet::load(provider, true)
            .map_err(|e| RgbError::StashLoad(format!("wallet load: {e}")))?;
        Ok(RgbWallet::new(stock, wallet))
    }

    /// Construct an Electrum-backed witness resolver pinned to the configured
    /// network. Mirrors `rgb-cmd`'s `RgbArgs::resolver`. Consumed by
    /// `validate_incoming_consignment` + the swap-PSBT methods (next session).
    #[allow(dead_code)]
    fn resolver(&self) -> Result<AnyResolver, RgbError> {
        let network = self.parse_network()?;
        let resolver = AnyResolver::electrum_blocking(&self.electrum_url, None)
            .map_err(|e| RgbError::StashLoad(format!("resolver: {e}")))?;

        resolver
            .check_chain_net(chain_net_for(network))
            .map_err(|e| RgbError::StashLoad(format!("resolver chain check: {e}")))?;

        Ok(resolver)
    }

    /// Load the maker's signing account (xpriv) from the configured encrypted
    /// file. Parallel to `load_wallet()` but for the hot side of the cold/hot
    /// split. Consumed by the swap-PSBT methods to sign the maker's own inputs.
    #[allow(dead_code)]
    fn load_signer(&self) -> Result<XprivAccount, RgbError> {
        XprivAccount::read(&self.signer_account_file, &self.signer_password)
            .map_err(|e| RgbError::TransferBuild(format!("load signer account: {e}")))
    }

    /// Refresh the on-disk bp-wallet UTXO cache against electrum. Without
    /// this, `load_wallet`'s cache stays frozen at whatever was last
    /// persisted; the maker's own swap-tx change outputs + any incoming
    /// sells stay invisible to `list_inventory_utxos` /
    /// `list_btc_only_utxos` until something calls back here. The
    /// maker-node's chain-observer loop drives this on a timer (issue #27).
    ///
    /// Uses `wallet.update(&AnyIndexer::Electrum(...))` — an incremental
    /// scan against the wallet's existing descriptor terminals, NOT a
    /// full `sync_from_scratch`. Persists via `Wallet`'s autosave-on-drop
    /// (the wallet handle is dropped at end of fn).
    pub async fn sync_wallet(&self) -> Result<(), RgbError> {
        let _guard = self.guard();
        use bpwallet::AnyIndexer;
        // bp-electrum exports its lib as `electrum` (vs `electrum-client`'s
        // `electrum_client`); they're distinct crates. bp-wallet's
        // `AnyIndexer::Electrum` variant takes the bp-electrum `Client`.
        let mut wallet = self.load_wallet()?;
        let client = electrum::Client::new(&self.electrum_url)
            .map_err(|e| RgbError::StashLoad(format!("electrum connect: {e}")))?;
        let indexer = AnyIndexer::Electrum(Box::new(client));
        let result = wallet.wallet_mut().update(&indexer);
        // bp-wallet `update` returns `MayError<(), Vec<Error>>` — partial
        // failures bubble through via `err`. Treat any error as a hard
        // failure for now; the observer loop logs + retries on the next tick.
        if let Some(errors) = result.err {
            return Err(RgbError::StashLoad(format!(
                "wallet sync surfaced {} error(s): {:?}",
                errors.len(),
                errors
            )));
        }
        Ok(())
    }

    /// Full from-scratch rescan (vs `sync_wallet`'s incremental `update`).
    /// Rebuilds the UTXO cache by re-deriving every keychain from index 0,
    /// applying the descriptor's stored tapret tweaks (the derive impl emits
    /// the plain key *and* every stored tweak per terminal). Recovers tapret
    /// host outputs the incremental scan stranded — within the indexer's gap
    /// limit (`BATCH_SIZE` consecutive-empty cutoff), so sparse high indexes
    /// past the gap may still be missed. Returns the count of tracked UTXOs
    /// after the rescan so callers can report recovery.
    pub async fn rescan_wallet(&self) -> Result<usize, RgbError> {
        let _guard = self.guard();
        use bpwallet::AnyIndexer;
        let mut wallet = self.load_wallet()?;
        let client = electrum::Client::new(&self.electrum_url)
            .map_err(|e| RgbError::StashLoad(format!("electrum connect: {e}")))?;
        let indexer = AnyIndexer::Electrum(Box::new(client));
        let result = wallet.wallet_mut().sync_from_scratch(&indexer);
        if let Some(errors) = result.err {
            return Err(RgbError::StashLoad(format!(
                "wallet rescan surfaced {} error(s): {:?}",
                errors.len(),
                errors
            )));
        }
        Ok(wallet.wallet().utxos().count())
    }

    /// Recover stranded RGB: sweep the maker's `stranded` outputs (which
    /// bp-wallet doesn't track, so normal selection can't reach them) plus one
    /// BTC `fee_input` into a single fresh tapret host output at the PINNED
    /// index — re-homing all the RGB onto a UTXO the wallet will recognise.
    /// Each input is `(outpoint, value_sats, scriptPubkey)` as read from the
    /// chain; the caller broadcasts the returned raw tx. Returns
    /// `(raw_tx, witness_txid)`. The stranded outpoints must still be unspent
    /// and hold live allocations in the stock (use the `inventory --btc`
    /// diagnostic to enumerate them).
    pub async fn recover_stranded_rgb(
        &self,
        asset: &AssetId,
        stranded: Vec<(Outpoint, u64, Vec<u8>)>,
        fee_input: (Outpoint, u64, Vec<u8>),
        fee_sats: u64,
    ) -> Result<(Vec<u8>, String, Vec<Outpoint>), RgbError> {
        let _guard = self.guard();
        let account = self.load_signer()?;
        let mut wallet = self.load_wallet()?;
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;

        let stranded_inputs: Vec<swap::RecoveryInput> = stranded
            .into_iter()
            .map(
                |(outpoint, value_sats, script_pubkey)| swap::RecoveryInput {
                    outpoint,
                    value_sats,
                    script_pubkey,
                },
            )
            .collect();
        let fee = swap::RecoveryInput {
            outpoint: fee_input.0,
            value_sats: fee_input.1,
            script_pubkey: fee_input.2,
        };

        let (raw_tx, witness_id, swept) = swap::build_recovery_sweep(
            &mut wallet,
            &account,
            contract_id,
            &stranded_inputs,
            &fee,
            fee_sats,
        )?;
        Ok((raw_tx, witness_id.to_string(), swept))
    }

    /// Single-contract RGB denomination-ladder split: spend one fat RGB UTXO
    /// (`rgb_source`) + one BTC `fee_input`, re-allocating the asset across
    /// `rungs` fresh keychain-0 outputs (largest→smallest), remainder back on the
    /// pinned tapret host. Builds, signs and finalizes under the wallet guard but
    /// does NOT broadcast (the std guard cannot cross an `.await`) — like
    /// `recover_stranded_rgb`, it returns the raw witness tx + txid for the caller
    /// to broadcast via its `BitcoinClient`. This is the Phase-1 spike kernel that
    /// the multi-contract `split_pools` generalises.
    pub async fn split_asset(
        &self,
        asset: &AssetId,
        rgb_source: (Outpoint, u64, Vec<u8>),
        fee_input: (Outpoint, u64, Vec<u8>),
        rungs: Vec<u64>,
        fee_sats: u64,
    ) -> Result<(Vec<u8>, String), RgbError> {
        let _guard = self.guard();
        let account = self.load_signer()?;
        let mut wallet = self.load_wallet()?;
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;
        let to_input =
            |(outpoint, value_sats, script_pubkey): (Outpoint, u64, Vec<u8>)| swap::RecoveryInput {
                outpoint,
                value_sats,
                script_pubkey,
            };
        let (raw_tx, witness_id) = swap::build_asset_split(
            &mut wallet,
            &account,
            contract_id,
            &to_input(rgb_source),
            &to_input(fee_input),
            &rungs,
            fee_sats,
        )?;
        Ok((raw_tx, witness_id.to_string()))
    }

    /// Multi-asset + BTC denomination rebalance: build ONE transaction that
    /// splits every listed RGB asset into its ladder rungs and (optionally) the
    /// BTC pool into its rungs, under a single tapret commitment + one fee. Each
    /// `assets` entry is `(asset, source_outpoint, rgb_rungs)`; `btc` is
    /// `(source_outpoint, sat_rungs)`. Sources are wallet-tracked Available UTXOs
    /// resolved from the coin cache. Builds/signs/finalizes under the wallet guard
    /// and returns `(raw_tx, txid)` for the caller to broadcast (no broadcast
    /// under the std guard). When `assets` is empty it falls back to a plain BTC
    /// split. Generalises [`Self::split_asset`].
    pub async fn split_pools(
        &self,
        assets: Vec<(AssetId, Outpoint, Vec<u64>)>,
        btc: Option<(Outpoint, Vec<u64>)>,
        fee_sats: u64,
    ) -> Result<(Vec<u8>, String), RgbError> {
        let _guard = self.guard();
        let account = self.load_signer()?;
        let mut wallet = self.load_wallet()?;

        let mut asset_inputs = Vec::with_capacity(assets.len());
        for (asset, source, rungs) in assets {
            let contract_id = ContractId::from_str(&asset.id)
                .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;
            asset_inputs.push(swap::AssetSplitInput {
                contract_id,
                source,
                rungs,
            });
        }

        let (raw_tx, witness_id) = if asset_inputs.is_empty() {
            let (op, rungs) = btc.as_ref().ok_or_else(|| {
                RgbError::TransferBuild("split_pools: nothing to split".to_owned())
            })?;
            swap::build_btc_split(&mut wallet, &account, op, rungs, fee_sats)?
        } else {
            let btc_source = btc.as_ref().map(|(op, _)| op);
            let empty: Vec<u64> = Vec::new();
            let btc_rungs = btc.as_ref().map(|(_, r)| r.as_slice()).unwrap_or(&empty);
            swap::build_rebalance_tx(
                &mut wallet,
                &account,
                &asset_inputs,
                btc_source,
                btc_rungs,
                fee_sats,
            )?
        };
        Ok((raw_tx, witness_id.to_string()))
    }

    /// Sync the wallet against electrum, then return its tracked UTXOs (BTC).
    /// Role-agnostic — needs no RGB contract; sum `sats` for the spendable
    /// balance, and the keychain distinguishes the BTC (0) vs RGB-anchor (10)
    /// addresses. Mirrors `sync_wallet`'s incremental update.
    pub async fn wallet_balance(&self) -> Result<Vec<WalletUtxo>, RgbError> {
        let _guard = self.guard();
        use bpstd::IdxBase as _;
        use bpwallet::AnyIndexer;
        let mut wallet = self.load_wallet()?;
        let client = electrum::Client::new(&self.electrum_url)
            .map_err(|e| RgbError::StashLoad(format!("electrum connect: {e}")))?;
        let indexer = AnyIndexer::Electrum(Box::new(client));
        let result = wallet.wallet_mut().update(&indexer);
        if let Some(errors) = result.err {
            return Err(RgbError::StashLoad(format!(
                "wallet sync surfaced {} error(s): {:?}",
                errors.len(),
                errors
            )));
        }
        let mut out = Vec::new();
        for u in wallet.wallet().utxos() {
            out.push(WalletUtxo {
                txid: u.outpoint.txid.to_string(),
                vout: u.outpoint.vout.into_u32(),
                sats: u.value.sats(),
                keychain: u.terminal.keychain.index(),
                index: u.terminal.index.index(),
            });
        }
        Ok(out)
    }

    /// Build a unilateral RGB transfer to `recipient_invoice`, returning the
    /// base64 consignment. This is the taker's sell-side action: it hands the
    /// maker a consignment that the maker validates and anchors into the swap
    /// tx. The taker does NOT broadcast the PSBT `pay()` returns — only the
    /// maker broadcasts, via its own swap composition. `wallet.pay` is the same
    /// primitive `rgb transfer` (rgb-cmd) uses; the wallet records the
    /// (tentative) transfer in its stash and autosaves on drop.
    pub async fn create_transfer_to_invoice(
        &self,
        recipient_invoice: &str,
        fee_sats: u64,
    ) -> Result<String, RgbError> {
        let invoice =
            RgbInvoice::from_str(recipient_invoice).map_err(|_| RgbError::InvalidInvoice)?;
        // `min_amount` is the witness-vout beneficiary dust floor; maker swap
        // invoices use blinded seals (no witness vout), so 546 is a safe nominal.
        let params = TransferParams::with(Sats(fee_sats), Sats(546));
        // Serialize against every other wallet mutation (chain observer sync,
        // swap settlement, rebalance self-send) — `pay` writes a tentative
        // transfer into the stash and autosaves on drop. No `.await` follows, so
        // the std guard can live to the end of the (otherwise synchronous) body.
        let _guard = self.guard();
        let mut wallet = self.load_wallet()?;
        let (_psbt, _meta, transfer) = wallet
            .pay(&invoice, params)
            .map_err(|e| RgbError::TransferBuild(format!("pay: {e}")))?;
        let mut bytes = Vec::new();
        transfer
            .save(&mut bytes)
            .map_err(|e| RgbError::TransferBuild(format!("serialize consignment: {e}")))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    /// Distribute RGB to a recipient invoice — the issuer's "send tokens" path.
    /// Builds the transfer (`wallet.pay`), signs all the issuer's inputs,
    /// finalizes, and **broadcasts** the anchoring tx (unlike
    /// `create_transfer_to_invoice`, which only builds a swap consignment for the
    /// maker to anchor). Returns `(witness_txid, base64 consignment)`; the
    /// recipient accepts the consignment once the tx confirms. Needs the signing
    /// account + a reachable electrum.
    pub async fn distribute(
        &self,
        recipient_invoice: &str,
        fee_sats: u64,
    ) -> Result<(String, String), RgbError> {
        let invoice =
            RgbInvoice::from_str(recipient_invoice).map_err(|_| RgbError::InvalidInvoice)?;
        let params = TransferParams::with(Sats(fee_sats), Sats(546));

        // Hold the wallet lock only for the synchronous build/sign/finalize: the
        // std `MutexGuard` is not `Send` and cannot cross the `.await` broadcast
        // below. We drop the wallet inside the guarded scope so its updated
        // stock/UTXO state autosaves *before* the lock is released — at which
        // point a concurrent chain-observer sync or swap can safely load it. The
        // autosave therefore lands just before broadcast rather than just after
        // (the prior, unguarded order); the window is tiny and a failed broadcast
        // leaves a diagnosable tentative transfer in the stash.
        let (raw_tx, witness_id, transfer) = {
            let _guard = self.guard();
            let mut wallet = self.load_wallet()?;
            let descriptor = wallet.wallet().descriptor().clone();
            let (mut psbt, _meta, transfer) = wallet
                .pay(&invoice, params)
                .map_err(|e| RgbError::TransferBuild(format!("pay: {e}")))?;

            // The issuer owns every input of a distribution tx, so it signs the
            // whole PSBT; then reuse the swap finalize/extract to get the
            // broadcast-ready tx.
            let account = self.load_signer()?;
            psbt.sign(&TestnetRefSigner::new(&account))
                .map_err(|e| RgbError::TransferBuild(format!("sign: {e}")))?;
            let (raw_tx, witness_id) = swap::finalize_signed_psbt(&descriptor, &psbt.to_base64())?;
            drop(wallet);
            (raw_tx, witness_id, transfer)
        };

        ElectrumClient::connect(&self.electrum_url)
            .map_err(|e| RgbError::TransferBuild(format!("electrum connect: {e}")))?
            .broadcast(&raw_tx)
            .await
            .map_err(|e| RgbError::TransferBuild(format!("broadcast: {e}")))?;

        let mut bytes = Vec::new();
        transfer
            .save(&mut bytes)
            .map_err(|e| RgbError::TransferBuild(format!("serialize consignment: {e}")))?;
        Ok((
            witness_id.to_string(),
            base64::engine::general_purpose::STANDARD.encode(bytes),
        ))
    }

    /// Re-derive the consignment for an already-settled transfer — the recovery
    /// fallback when a recipient lost theirs (failed delivery, wallet reset, missed
    /// import). Unlike [`Self::distribute`], this builds NOTHING new and touches no
    /// chain: the transition + witness already live in the stash (the original
    /// swap's `commit_and_consign` consumed the fascia), so `stock.transfer` simply
    /// re-extracts the witness-extended consignment addressed to `outpoint`'s seal.
    /// Read-only — no signing, no broadcast, no persistence.
    ///
    /// Witness-vout (explicit `txid:vout`) seals only — what swaps use; the witness
    /// id is the outpoint's txid. Blinded/secret seals can't be re-derived (their
    /// outpoint is hidden), so those aren't recoverable this way. Returns the base64
    /// consignment for the recipient to re-import.
    pub fn reconsign(&self, contract: &str, outpoint: &str) -> Result<String, RgbError> {
        let contract_id = ContractId::from_str(contract)
            .map_err(|e| RgbError::TransferBuild(format!("bad contract id {contract}: {e}")))?;
        let output_seal = OutputSeal::from_str(outpoint)
            .map_err(|e| RgbError::TransferBuild(format!("bad outpoint {outpoint}: {e}")))?;
        // Witness-vout delivery: the seal's txid IS the witness tx the consignment
        // anchors to (same identity `commit_and_consign` relies on).
        let witness_id = output_seal.txid;

        let stock = self.load_stock()?;
        let transfer = stock
            .transfer(contract_id, [output_seal], [], [], Some(witness_id))
            .map_err(|e| RgbError::TransferBuild(format!("reconsign transfer: {e}")))?;

        let mut bytes = Vec::new();
        transfer
            .save(&mut bytes)
            .map_err(|e| RgbError::TransferBuild(format!("serialize consignment: {e}")))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    /// Export the **provenance** of the caller's own allocation(s) at `outpoints` —
    /// the consignment a holder hands a counterparty so the counterparty can spend
    /// that allocation in a transaction *it* builds (the sell leg of a swap). Unlike
    /// a `wallet.pay` consignment it **spends nothing, touches no chain, and
    /// references no counterparty seal**: it terminates on the holder's *own*
    /// witness-vout outpoints. Generalizes [`Self::reconsign`] to multiple outpoints.
    /// See `docs/provenance-consignment-proposal.md`. Witness-vout (explicit
    /// `txid:vout`) seals only. Returns the base64 consignment.
    pub fn export_provenance(
        &self,
        contract: &str,
        outpoints: &[String],
    ) -> Result<String, RgbError> {
        let _guard = self.guard();
        let contract_id = ContractId::from_str(contract)
            .map_err(|e| RgbError::TransferBuild(format!("bad contract id {contract}: {e}")))?;
        let seals: Vec<OutputSeal> = outpoints
            .iter()
            .map(|o| {
                OutputSeal::from_str(o)
                    .map_err(|e| RgbError::TransferBuild(format!("bad outpoint {o}: {e}")))
            })
            .collect::<Result<_, _>>()?;
        if seals.is_empty() {
            return Err(RgbError::TransferBuild(
                "export_provenance: no outpoints supplied".to_owned(),
            ));
        }
        // `witness_id` MUST be None here. It would be tempting to pass the seal's
        // txid (the `reconsign` shortcut), but that only equals the witness tx for
        // witness-vout seals. When the allocation was *received* on a blinded seal
        // bound to a pre-existing outpoint, the seal's txid is that prior UTXO, while
        // the transition lives on a *different* witness tx — and `Stock::consign`
        // filters out every bundle not anchored to `witness_id`, yielding an EMPTY
        // consignment whose accept registers nothing. None lets `consign` resolve the
        // bundle from the outpoint (`opouts_by_outputs`) and walk the full graph.
        let stock = self.load_stock()?;
        let transfer = stock
            .transfer(contract_id, seals, [], [], None)
            .map_err(|e| RgbError::TransferBuild(format!("export provenance transfer: {e}")))?;

        let mut bytes = Vec::new();
        transfer
            .save(&mut bytes)
            .map_err(|e| RgbError::TransferBuild(format!("serialize consignment: {e}")))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    /// Wallet-derived BTC inventory: every spendable wallet UTXO that is
    /// **not** carrying an RGB allocation for `asset`. Replaces the hardcoded
    /// `mock_btc_inventory()` in `maker-node/src/main.rs`. Each returned
    /// [`BtcInventoryUtxo`] is `Available` (the maker's reservation state
    /// machine assigns the other statuses).
    ///
    /// scriptPubkey is derived from the wallet descriptor + the UTXO's
    /// terminal — same path `swap::enrich_psbt_input` uses on the maker
    /// side, so the bytes round-trip cleanly into PSBT inputs.
    /// Wallet UTXOs that carry NO RGB for ANY of `assets` — the maker's spendable
    /// BTC pool. Multi-asset: every registered contract's allocations are excluded,
    /// so an output bound to asset B is never handed back as plain BTC when the
    /// maker also trades asset A (which would let a swap/fee spend destroy B's RGB).
    pub async fn list_btc_only_utxos(
        &self,
        assets: &[AssetId],
        now_ms: u64,
    ) -> Result<Vec<rfq_types::BtcInventoryUtxo>, RgbError> {
        let _guard = self.guard();
        use bpstd::Derive as _;
        let wallet = self.load_wallet()?;

        // Build the set of outpoints carrying RGB for ANY traded contract in this
        // maker's wallet (matches `list_inventory_utxos`'s per-contract filter,
        // unioned across all assets).
        let mut rgb_outpoints: std::collections::HashSet<(String, u32)> =
            std::collections::HashSet::new();
        for asset in assets {
            let contract_id = ContractId::from_str(&asset.id)
                .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;
            if let Ok(contract) = wallet.stock().contract_data(contract_id) {
                let filter = FilterIncludeAll;
                for details in contract.schema.owned_types.values() {
                    if let Ok(allocs) = contract.fungible(details.name.clone(), &filter) {
                        for alloc in allocs {
                            let op = alloc.seal.to_outpoint();
                            rgb_outpoints.insert((op.txid.to_string(), op.vout.into_u32()));
                        }
                    }
                }
            }
        }

        let descriptor = wallet.wallet().descriptor().clone();
        let mut btc = Vec::new();
        for u in wallet.wallet().utxos() {
            let key = (u.outpoint.txid.to_string(), u.outpoint.vout.into_u32());
            if rgb_outpoints.contains(&key) {
                continue;
            }
            let derived = descriptor
                .derive(u.terminal.keychain, u.terminal.index)
                .next()
                .ok_or_else(|| {
                    RgbError::TransferBuild("descriptor produced no script".to_owned())
                })?;
            let spk: Vec<u8> = derived.to_script_pubkey().as_slice().to_vec();
            btc.push(rfq_types::BtcInventoryUtxo {
                outpoint: Outpoint {
                    txid: key.0,
                    vout: key.1,
                },
                value_sats: u.value.sats(),
                script_pubkey: spk,
                status: rfq_types::BtcInventoryStatus::Available,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                pending_txid: None,
            });
        }
        Ok(btc)
    }

    /// Diagnostic/test: the wallet terminal `(keychain, index)` a tracked UTXO
    /// derives from, or `None` if bp-wallet doesn't track the outpoint. Used to
    /// assert the maker's tapret change lands on the pinned host terminal.
    pub async fn debug_outpoint_terminal(
        &self,
        outpoint: &Outpoint,
    ) -> Result<Option<(u32, u32)>, RgbError> {
        use bpstd::IdxBase as _;
        let _guard = self.guard();
        let wallet = self.load_wallet()?;
        let terminal = wallet
            .wallet()
            .utxos()
            .find(|u| {
                u.outpoint.txid.to_string() == outpoint.txid
                    && u.outpoint.vout.into_u32() == outpoint.vout
            })
            .map(|u| (u.terminal.keychain.index(), u.terminal.index.index()));
        Ok(terminal)
    }

    /// Diagnostic: every **live** allocation the RGB stock holds for `asset`,
    /// each tagged with whether its outpoint is in the wallet's UTXO set.
    /// Unlike `list_inventory_utxos` — which silently drops non-owned
    /// allocations — this surfaces them, so "the maker has no RGB inventory"
    /// can be split into *depletion* (no live allocation at all) vs *lost
    /// change* (a live allocation exists but on an outpoint bp-wallet isn't
    /// tracking → `in_wallet == false`). Returns `(outpoint, amount, in_wallet)`.
    pub async fn debug_contract_allocations(
        &self,
        asset: &AssetId,
    ) -> Result<Vec<(Outpoint, u64, bool)>, RgbError> {
        let _guard = self.guard();
        let wallet = self.load_wallet()?;
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;
        let contract = wallet
            .stock()
            .contract_data(contract_id)
            .map_err(|e| RgbError::ContractNotFound(e.to_string()))?;

        let owned: std::collections::HashSet<(String, u32)> = wallet
            .wallet()
            .utxos()
            .map(|u| (u.outpoint.txid.to_string(), u.outpoint.vout.into_u32()))
            .collect();

        let mut out = Vec::new();
        let filter = FilterIncludeAll;
        for details in contract.schema.owned_types.values() {
            if let Ok(allocs) = contract.fungible(details.name.clone(), &filter) {
                for alloc in allocs {
                    let op = alloc.seal.to_outpoint();
                    let key = (op.txid.to_string(), op.vout.into_u32());
                    let in_wallet = owned.contains(&key);
                    out.push((
                        Outpoint {
                            txid: key.0,
                            vout: key.1,
                        },
                        alloc.state.value(),
                        in_wallet,
                    ));
                }
            }
        }
        Ok(out)
    }

    #[allow(dead_code)]
    fn parse_network(&self) -> Result<Network, RgbError> {
        match self.network.as_str() {
            "mainnet" | "bitcoin" => Ok(Network::Mainnet),
            "regtest" => Ok(Network::Regtest),
            "signet" => Ok(Network::Signet),
            "testnet" | "testnet3" => Ok(Network::Testnet3),
            "testnet4" => Ok(Network::Testnet4),
            other => Err(RgbError::StashLoad(format!("unknown network `{other}`"))),
        }
    }

    /// Confirmation depth K the mined-ancestry gates require before treating a witness as
    /// final. Network-aware via the single policy in `rfq_consignment::Network`
    /// (mainnet 6, testnet 3, signet/regtest 1); unknown label falls back to 1.
    fn min_confs(&self) -> u32 {
        rfq_consignment::Network::from_label(&self.network)
            .map(|n| n.recommended_confs())
            .unwrap_or(1)
    }
}

/// A single wallet UTXO: BTC value plus its derivation terminal. Role-agnostic
/// (no RGB contract needed); `keychain` is the BIP-86 keychain (0 = BTC,
/// 10 = tapret RGB anchor).
#[derive(Debug, Clone)]
pub struct WalletUtxo {
    pub txid: String,
    pub vout: u32,
    pub sats: u64,
    pub keychain: u32,
    pub index: u32,
}

#[allow(dead_code)]
fn chain_net_for(network: Network) -> ChainNet {
    match network {
        Network::Mainnet => ChainNet::BitcoinMainnet,
        Network::Regtest => ChainNet::BitcoinRegtest,
        Network::Signet => ChainNet::BitcoinSignet,
        Network::Testnet3 => ChainNet::BitcoinTestnet3,
        Network::Testnet4 => ChainNet::BitcoinTestnet4,
    }
}

/// Enumerate every witness txid in a consignment, **structurally** — decode the base64
/// `Transfer` and read each `WitnessBundle`'s witness id. No stock, resolver, or RGB
/// validation: this is the cheap txid extraction the broker precheck and the SPV-prover
/// distribution endpoint need (a caller that ALSO validates obtains the same set from the
/// validation status' `tx_ord_map`). Returns display-order txid hex, deduplicated.
///
/// This is a *liveness* helper, not a trust boundary: the txids feed a mined-ancestry
/// check (`rfq-consignment`) which is where soundness is actually enforced. A consignment
/// that decodes here but lies about its graph is caught there and by the maker's own gate.
pub fn witness_txids_of_consignment(consignment_base64: &str) -> Result<Vec<String>, RgbError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(consignment_base64.trim())
        .map_err(|e| RgbError::TransferBuild(format!("consignment is not valid base64: {e}")))?;
    let consignment = Transfer::load(bytes.as_slice())
        .map_err(|e| RgbError::TransferBuild(format!("consignment decode: {e}")))?;
    let mut seen = std::collections::HashSet::new();
    let mut txids = Vec::new();
    for wb in consignment.bundled_witnesses() {
        let txid = wb.witness_id().to_string();
        if seen.insert(txid.clone()) {
            txids.push(txid);
        }
    }
    Ok(txids)
}

#[async_trait]
impl RgbBackend for LibRgbBackend {
    async fn asset_spec(&self, asset: &AssetId) -> Result<(String, u8), RgbError> {
        // Reuse the sync inherent reader (loads stock, reads the contract `spec`).
        self.contract_spec(asset)
    }

    async fn list_inventory_utxos(
        &self,
        asset: &AssetId,
    ) -> Result<Vec<RgbInventoryUtxo>, RgbError> {
        let _guard = self.guard();
        // NB: the wallet UTXO set here is the on-disk cache — `load_wallet` does
        // no network sync. A long-running maker-node must refresh it in its
        // lifecycle (see "Wallet UTXO-cache sync" in docs/broker-maker-node-roadmap.md).
        let wallet = self.load_wallet()?;
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;
        let contract = wallet
            .stock()
            .contract_data(contract_id)
            .map_err(|e| RgbError::ContractNotFound(e.to_string()))?;

        // The contract state carries every allocation the stash has seen —
        // including the issuer's and counterparties'. Inventory is only what
        // *this* maker can spend, so keep allocations whose outpoint is one of
        // the wallet's own UTXOs. The map also carries each UTXO's bitcoin value
        // so we can surface `btc_sats` (the seal-anchor sats spent on the swap).
        let owned: std::collections::HashMap<(String, u32), u64> = wallet
            .wallet()
            .utxos()
            .map(|u| {
                (
                    (u.outpoint.txid.to_string(), u.outpoint.vout.into_u32()),
                    u.value.sats(),
                )
            })
            .collect();

        let mut utxos = Vec::new();
        let filter = FilterIncludeAll;
        for details in contract.schema.owned_types.values() {
            if let Ok(rgb_allocations) = contract.fungible(details.name.clone(), &filter) {
                for alloc in rgb_allocations {
                    let op = alloc.seal.to_outpoint();
                    let key = (op.txid.to_string(), op.vout.into_u32());
                    let Some(&btc_sats) = owned.get(&key) else {
                        continue;
                    };
                    utxos.push(RgbInventoryUtxo {
                        outpoint: Outpoint {
                            txid: key.0,
                            vout: key.1,
                        },
                        asset_id: asset.clone(),
                        amount: alloc.state.value(),
                        btc_sats,
                    });
                }
            }
        }
        Ok(utxos)
    }

    async fn validate_invoice(&self, invoice: &str) -> Result<(), RgbError> {
        RgbInvoice::from_str(invoice)
            .map(|_| ())
            .map_err(|_| RgbError::InvalidInvoice)
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_swap_psbt_buy(
        &self,
        rgb_invoice: &str,
        amount: u64,
        maker_rgb_utxos: &[Outpoint],
        taker_btc_inputs: &[(Outpoint, TxOut)],
        _btc_funding_addr: &str,
        gross_btc_sats: u64,
        actual_fee_sats: u64,
        change_rungs: &[u64],
    ) -> Result<SwapTransfer, RgbError> {
        let _guard = self.guard();
        let account = self.load_signer()?;
        let mut wallet = self.load_wallet()?;

        // 1. Parse the taker's invoice and resolve the maker's payout address +
        //    reserved RGB inputs. `change_rungs` (if any) reserves fresh k0
        //    outputs that piggyback a denomination-ladder split of the maker's
        //    RGB change onto this settlement tx.
        let inputs = swap::prepare_buy_inputs(
            &mut wallet,
            rgb_invoice,
            maker_rgb_utxos,
            change_rungs.len(),
        )?;

        // 2. Assemble the unsigned swap tx (maker RGB + taker BTC inputs) with the
        //    commitment host; from here the witness txid is final.
        let mut psbt = swap::assemble_unsigned_psbt(
            &inputs,
            taker_btc_inputs,
            gross_btc_sats,
            actual_fee_sats,
        )?;

        // 3. Build the RGB transition: pay the taker's seal (blinded or
        //    witness-vout), change back to the maker (split into `change_rungs`
        //    rungs + a host remainder). `delivery` carries how to address the
        //    taker's consignment.
        let (batch, delivery) =
            swap::build_buy_transition(&wallet, &inputs, &psbt, amount, change_rungs)?;

        // 4. Embed + commit the transition and emit the witness-extended consignment.
        let (transfer, witness_id) =
            swap::commit_and_consign(&mut wallet, &mut psbt, batch, inputs.contract_id, delivery)?;

        // 5. Sign the maker's inputs (after commit — U4); taker BTC inputs stay open.
        swap::sign_maker_inputs(&mut psbt, &account)?;

        // 6. Encode the partial PSBT + consignment for the wire.
        swap::encode_swap_transfer_buy(&psbt, &transfer, witness_id)
    }

    async fn finalize_after_taker_sign(
        &self,
        signed_psbt_base64: &str,
        original_consignment_base64: &str,
    ) -> Result<FinalizedSwap, RgbError> {
        let _guard = self.guard();
        // Maker descriptor is needed to finalize the maker's own inputs
        // (sign_maker_inputs leaves them with partial_sigs only). The taker's
        // inputs should already be finalized on its end.
        let wallet = self.load_wallet()?;
        let descriptor = wallet.wallet().descriptor().clone();
        let (raw_tx, witness_id) = swap::finalize_signed_psbt(&descriptor, signed_psbt_base64)?;
        Ok(FinalizedSwap {
            raw_tx,
            witness_txid: witness_id.to_string(),
            // Consignment was witness-extended at PSBT-build (D3); re-emitting
            // via stock.transfer here would duplicate. See
            // docs/swap-psbt-design.md §finalize_after_taker_sign.
            final_consignment_base64: original_consignment_base64.to_owned(),
        })
    }

    async fn create_invoice(&self, asset: &AssetId, amount: u64) -> Result<String, RgbError> {
        let _guard = self.guard();
        // Mirrors rgb-cmd's `Invoice` command for the fungible case: prefer a
        // blinded seal on a pre-existing keychain-10 anchor; when none is
        // available, fall back to a witness-vout "future seal" so the receiver
        // never runs out of anchors. Real cryptographic material — meaningful
        // only with a live regtest/electrum stack.
        let mut wallet = self.load_wallet()?;
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;

        // Pick a FREE UTXO on the RGB seal-anchor chain (BIP-389 keychain 10 —
        // the tapret descriptor terminal is `/<0;1;10>/*`, with 0 = receive,
        // 1 = change, 10 = tapret anchors). `Sats::ZERO` because the seal
        // *references* the UTXO; the invoice doesn't spend it.
        let network = wallet.wallet().network();

        // Exclude keychain-10 UTXOs that already carry an RGB allocation: a seal
        // anchor must be free, or it could be spent out from under the new seal
        // (e.g. the sell spends the very UTXO holding the RGB being sold, which
        // would orphan a change seal placed on it). Once every free anchor is
        // used, no UTXO qualifies and `create_invoice` falls back to witness-vout.
        let allocated: std::collections::HashSet<(String, u32)> = {
            let mut set = std::collections::HashSet::new();
            if let Ok(contract) = wallet.stock().contract_data(contract_id) {
                let filter = FilterIncludeAll;
                for details in contract.schema.owned_types.values() {
                    if let Ok(allocs) = contract.fungible(details.name.clone(), &filter) {
                        for alloc in allocs {
                            let op = alloc.seal.to_outpoint();
                            set.insert((op.txid.to_string(), op.vout.into_u32()));
                        }
                    }
                }
            }
            set
        };
        let anchor = wallet
            .wallet()
            .coinselect(Sats::ZERO, |utxo| {
                RgbKeychain::contains_rgb(utxo.terminal.keychain)
                    && !allocated.contains(&(
                        utxo.outpoint.txid.to_string(),
                        utxo.outpoint.vout.into_u32(),
                    ))
            })
            .next();

        let beneficiary = match anchor {
            // Preferred: a blinded seal bound to an existing anchor. Hides the
            // UTXO from the sender and needs no extra swap output. Mint a fresh
            // single-use seal and stash its secret so we can *open* it when
            // accepting the sender's consignment.
            Some(outpoint) => {
                let seal = GraphSeal::new_random(outpoint.txid, outpoint.vout);
                wallet
                    .stock_mut()
                    .store_secret_seal(seal)
                    .map_err(|e| RgbError::TransferBuild(format!("store seal: {e}")))?;
                Beneficiary::BlindedSeal(seal.to_secret_seal())
            }
            // Fallback: no free anchor → a witness-vout "future seal" on a fresh
            // keychain-10 (Tapret) address. Valid for a plain receive (the RGB lands
            // on a NEW output of the broadcast tx the sender builds). NOTE: this is
            // NOT used for the sell-side maker invoice anymore — sells use the
            // provenance pattern (the taker exports provenance + names its outpoints;
            // see docs/provenance-consignment-proposal.md), so the maker mints no
            // invoice at all and never needs a spare anchor.
            None => {
                let addr = wallet.wallet_mut().next_address(RgbKeychain::Tapret, true);
                Beneficiary::WitnessVout(Pay2Vout::new(addr.payload), None)
            }
        };

        // Pin the invoice to (network, beneficiary, contract, amount). The
        // resulting `RgbInvoice` string is what the taker passes to the
        // sender's transfer flow.
        let mut builder = RgbInvoiceBuilder::new(XChainNet::bitcoin(network, beneficiary))
            .set_contract(contract_id)
            .set_amount_raw(amount);

        // For NIA-like single-assignment fungible contracts the assignment
        // name is unambiguous; fall back to leaving it unset (the invoice
        // remains usable, just less strictly typed) for ambiguous schemas.
        if let Ok(contract) = wallet.stock().contract_data(contract_id) {
            let assignment_types = contract
                .schema
                .assignment_types_for_state(StateType::Fungible);
            if assignment_types.len() == 1 {
                let name = contract
                    .schema
                    .assignment_name(*assignment_types[0])
                    .clone();
                builder = builder.set_assignment_name(name);
            }
        }

        Ok(builder.finish().to_string())
    }

    async fn parse_maker_invoice(&self, invoice: &str) -> Result<MakerInvoiceParts, RgbError> {
        // The maker minted this invoice via `create_invoice`; we only need its
        // contract id + amount. The beneficiary seal is irrelevant — the maker
        // routes its own receive to a fresh witness-vout output in
        // `build_sell_transition`, so the invoice (blinded OR witness-vout, e.g.
        // when the maker has no free anchor) is just the taker's consignment
        // target.
        let parsed = RgbInvoice::from_str(invoice).map_err(|_| RgbError::InvalidInvoice)?;
        let contract_id = parsed.contract.ok_or_else(|| {
            RgbError::TransferBuild("maker invoice carries no contract id".to_owned())
        })?;
        let amount = match parsed.assignment_state {
            Some(InvoiceState::Amount(a)) => Some(*a.as_inner()),
            _ => None,
        };
        Ok(MakerInvoiceParts {
            contract_id,
            amount,
        })
    }

    async fn validate_incoming_consignment(
        &self,
        consignment_base64: &str,
        expected_contract_id: ContractId,
        consigned_outpoints: &[Outpoint],
    ) -> Result<ConsignmentInfo, RgbError> {
        let _guard = self.guard();
        // Mirrors rgb-cmd's `Validate`/`Accept` path: decode → validate the
        // state transition against the resolver + the maker's typesystem →
        // cross-check the contract id matches the maker's invoice. The
        // base64 wraps the strict-encoded `Transfer` binary (the same bytes
        // `Transfer::save_file` produces).
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(consignment_base64.trim())
            .map_err(|e| {
                RgbError::TransferBuild(format!("consignment is not valid base64: {e}"))
            })?;

        let consignment = Transfer::load(bytes.as_slice())
            .map_err(|e| RgbError::TransferBuild(format!("consignment decode: {e}")))?;

        if consignment.contract_id() != expected_contract_id {
            return Err(RgbError::TransferBuild(format!(
                "consignment contract {} does not match expected {}",
                consignment.contract_id(),
                expected_contract_id
            )));
        }

        // SECURITY (sell-side gate): REQUIRE every witness in the consigned history to be
        // mined on-chain BEFORE the maker builds/signs/broadcasts the swap — otherwise a
        // fabricated history riding never-mined witness txs would validate and the maker
        // would pay BTC for RGB that never existed. Graph-validate with a SEEDED resolver
        // (so the validator has the full witness graph), then enforce mining explicitly via
        // the shared `MinedChecker` (every witness must be mined — no exemptions on a sell;
        // the swap tx is built afterwards). `mut` stock because we `accept_transfer` below;
        // `Stock::load(_, true)` autosaves on drop.
        let mut stock = self.load_stock()?;

        let mut resolver = self.resolver()?;
        resolver.add_consignment_txes(&consignment);

        let validation_config = ValidationConfig {
            chain_net: chain_net_for(self.parse_network()?),
            trusted_typesystem: stock
                .as_stash_provider()
                .type_system()
                .map_err(|e| RgbError::StashLoad(format!("type system: {e}")))?
                .clone(),
            ..Default::default()
        };

        let validated = consignment
            .validate(&resolver, &validation_config)
            .map_err(|e| RgbError::TransferBuild(format!("consignment validation: {e:?}")))?;

        if validated.validation_status().validity() != Validity::Valid {
            return Err(RgbError::TransferBuild(format!(
                "consignment invalid: {}",
                validated.validation_status()
            )));
        }

        // Mined-ancestry gate (pipelined). Reject BEFORE `accept_transfer` so a bad
        // consignment never mutates the stash. K is network-aware (`self.min_confs()`).
        let txids: Vec<String> = validated
            .validation_status()
            .tx_ord_map
            .keys()
            .map(|t| t.to_string())
            .collect();
        let verdict = MinedChecker::new(vec![self.electrum_url.clone()], self.min_confs())
            .check(&txids, &HashSet::new())
            .map_err(|e| RgbError::TransferBuild(format!("mined-ancestry check: {e}")))?;
        if !verdict.all_mined {
            return Err(RgbError::TransferBuild(format!(
                "consignment ancestry not mined on-chain: {:?}",
                verdict.unmined
            )));
        }

        // Absorb the consignment so the taker's allocations + their full history
        // enter the maker's stash; `create_swap_psbt_sell` then spends them into the
        // maker's seal. A provenance consignment delivers to the taker's OWN
        // outpoints, which the taker names on the wire (`consigned_outpoints`) — the
        // consignment records provenance, not which outpoints the holder is offering
        // (see docs/provenance-consignment-proposal.md). Accept first, then confirm.
        stock
            .accept_transfer(validated, &resolver)
            .map_err(|e| RgbError::StashLoad(format!("accept_transfer: {e}")))?;

        // Confirm the claimed RGB actually sits at the named outpoints, and sum it.
        // `contract.fungible` only surfaces allocations the stash truly holds, so a
        // taker naming an outpoint the consignment didn't convey contributes 0 — the
        // swap can't over-spend. The chain/ownership/signature checks happen upstream
        // (`get_outpoint` in rfq-maker + the taker's signature at /sign).
        let wanted: std::collections::HashSet<(String, u32)> = consigned_outpoints
            .iter()
            .map(|o| (o.txid.clone(), o.vout))
            .collect();
        let contract = stock
            .contract_data(expected_contract_id)
            .map_err(|e| RgbError::ContractNotFound(e.to_string()))?;
        let mut total_amount: u64 = 0;
        let filter = FilterIncludeAll;
        for details in contract.schema.owned_types.values() {
            if let Ok(allocs) = contract.fungible(details.name.clone(), &filter) {
                for alloc in allocs {
                    let op = alloc.seal.to_outpoint();
                    if wanted.contains(&(op.txid.to_string(), op.vout.into_u32())) {
                        total_amount = total_amount.saturating_add(alloc.state.value());
                    }
                }
            }
        }

        Ok(ConsignmentInfo {
            total_amount,
            outpoints: consigned_outpoints.to_vec(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_swap_psbt_sell(
        &self,
        consignment_info: &ConsignmentInfo,
        taker_rgb_prevouts: &[(Outpoint, TxOut)],
        maker_btc_inputs: &[(Outpoint, TxOut)],
        contract_id: ContractId,
        deliver_amount: u64,
        btc_payout_addr: &str,
        rgb_change_invoice: Option<&str>,
        gross_btc_sats: u64,
        actual_fee_sats: u64,
        btc_change_rungs: &[u64],
    ) -> Result<SwapTransfer, RgbError> {
        let _guard = self.guard();
        let account = self.load_signer()?;
        let mut wallet = self.load_wallet()?;

        // Robustness: bp-wallet's incremental `sync_wallet` can strand keychain-10 maker
        // BTC outpoints (tapret tweak + gap-limit / index reuse — see
        // project_maker_tapret_change_stranding), so a freshly loaded UTXO cache may be
        // missing a declared maker BTC input even though it's unspent on-chain. Recover
        // with in-memory from-scratch rescans, RETRIED (the rescan can transiently fail
        // against a busy electrs), until every declared input is present. We rescan the
        // already-loaded wallet directly rather than calling `rescan_wallet`, which would
        // re-acquire `self.guard()` and deadlock.
        if !maker_btc_inputs.is_empty() {
            use bpwallet::AnyIndexer;
            let have: HashSet<(String, u32)> = wallet
                .wallet()
                .utxos()
                .map(|u| (u.outpoint.txid.to_string(), u.outpoint.vout.into_u32()))
                .collect();
            let missing = maker_btc_inputs
                .iter()
                .any(|(op, _)| !have.contains(&(op.txid.clone(), op.vout)));
            if missing {
                if let Ok(client) = electrum::Client::new(&self.electrum_url) {
                    let indexer = AnyIndexer::Electrum(Box::new(client));
                    let _ = wallet.wallet_mut().sync_from_scratch(&indexer);
                }
            }
        }

        // 1. Resolve the maker's side: taker's RGB-change seal (if any), the
        //    taker's BTC payout spk, a fresh maker receive + change address,
        //    per-input terminals for the maker's BTC inputs, and (if any) fresh
        //    k0 outputs for piggybacking a BTC-change ladder split onto this tx.
        let inputs = swap::prepare_sell_inputs(
            &mut wallet,
            contract_id,
            deliver_amount,
            btc_payout_addr,
            rgb_change_invoice,
            maker_btc_inputs,
            btc_change_rungs.len(),
        )?;

        // 2. Assemble the unsigned swap tx (taker RGB + maker BTC inputs) with
        //    the commitment host; from here the witness txid is final. The maker's
        //    BTC change is split into `btc_change_rungs` + a remainder.
        let mut psbt = swap::assemble_sell_psbt(
            &inputs,
            taker_rgb_prevouts,
            gross_btc_sats,
            actual_fee_sats,
            btc_change_rungs,
        )?;

        // 3. Build the RGB transition against the sorted PSBT: spend the taker's
        //    allocations (visible since `validate_incoming_consignment` ran
        //    `accept_transfer`), deliver to the maker's seal, route any
        //    over-consigned surplus to the taker's change seal (blinded or
        //    witness-vout). `delivery` is how the taker's change consignment is
        //    addressed; `has_change` flags whether there's any.
        let (batch, delivery, has_change) =
            swap::build_sell_transition(&wallet, &inputs, &psbt, consignment_info)?;

        // 4. Embed + commit the transition and emit the witness-extended
        //    consignment to the taker's change seal so it can accept its RGB
        //    change post-broadcast (with no surplus the consignment is dropped).
        let (transfer, witness_id) =
            swap::commit_and_consign(&mut wallet, &mut psbt, batch, inputs.contract_id, delivery)?;

        // 5. Sign the maker's BTC inputs (after commit — U4); taker RGB inputs
        //    stay open for /sign.
        swap::sign_maker_inputs(&mut psbt, &account)?;

        // 6. Encode the partial PSBT for the wire, attaching the taker's
        //    change consignment when there was surplus.
        let change_consignment = has_change.then_some(&transfer);
        swap::encode_swap_transfer_sell(&psbt, witness_id, change_consignment)
    }

    async fn verify_signed_swap_psbt(
        &self,
        signed_psbt_base64: &str,
        consigned_outpoints: &[Outpoint],
        expected_witness_txid: &str,
    ) -> Result<(), RgbError> {
        // Decode the real PSBT and compare structurally — prevouts live as raw
        // (byte-reversed) bytes, never the ASCII `txid:vout` string the mock
        // embeds, so the maker's old substring scan never matched here.
        let psbt = Psbt::from_base64(signed_psbt_base64)
            .map_err(|e| RgbError::TransferBuild(format!("decode signed PSBT: {e}")))?;

        let spent: std::collections::HashSet<(String, u32)> = psbt
            .inputs()
            .map(|inp| {
                (
                    inp.previous_outpoint.txid.to_string(),
                    inp.previous_outpoint.vout.into_u32(),
                )
            })
            .collect();
        for outpoint in consigned_outpoints {
            if !spent.contains(&(outpoint.txid.clone(), outpoint.vout)) {
                return Err(RgbError::TransferBuild(
                    "input set diverged from consignment".to_owned(),
                ));
            }
        }

        // The unsigned-tx txid is fixed at build time (every input was committed
        // before signing — D3 invariant), so a divergent txid means tampering.
        if psbt.txid().to_string() != expected_witness_txid {
            return Err(RgbError::TransferBuild("txid mismatch".to_owned()));
        }
        Ok(())
    }
}
