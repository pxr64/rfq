use std::path::PathBuf;
use std::str::FromStr;

use async_trait::async_trait;
use rfq_types::{AssetId, Outpoint, RgbInventoryUtxo, SwapTransfer};

use bpstd::{Network, Sats, XprivAccount, XpubDerivable};
use bpwallet::fs::FsTextStore;
use bpwallet::hot::SecureIo;
use bpwallet::Wallet;
use amplify::Wrapper as _;
use base64::Engine as _;
use rgb::containers::{ConsignmentExt, FileContent, Transfer};
use rgb::contract::FilterIncludeAll;
use rgb::invoice::{Beneficiary, RgbInvoice, RgbInvoiceBuilder, XChainNet};
use rgb::persistence::fs::FsBinStore;
use rgb::persistence::{StashReadProvider, Stock};
use rgb::resolvers::AnyResolver;
use rgb::validation::{ResolveWitness, ValidationConfig, Validity};
use rgb::{
    ChainNet, ContractId, GraphSeal, RgbDescr, RgbKeychain, RgbWallet, StateType, TxoSeal,
};

use crate::swap;

use crate::{ConsignmentInfo, FinalizedSwap, RgbBackend, RgbError, TxOut};

/// Library-backed `RgbBackend` implementation. Talks directly to `rgb-api` /
/// `rgb-ops` (the same libraries `rgb-cmd` is built on) — no subprocess.
///
/// Stashes follow the rgb-cmd convention: `<data_dir>/<network>/...`. So
/// existing stashes created by `make rgb-wallets-init` + `rgb_<role> create
/// --wpkh ...` are reusable without re-import.
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
        }
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
        let provider = FsBinStore::new(self.stock_path())
            .map_err(|e| RgbError::StashLoad(e.to_string()))?;
        Stock::load(provider, true).map_err(|e| RgbError::StashLoad(e.to_string()))
    }

    /// Load the full RGB wallet: a `bp-wallet` `Wallet<XpubDerivable, RgbDescr>`
    /// wrapped with the maker's `Stock`. The wallet descriptor must already
    /// exist on disk (e.g. via `make rgb-wallets-init` + `rgb create --wpkh`).
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

#[async_trait]
impl RgbBackend for LibRgbBackend {
    async fn list_inventory_utxos(
        &self,
        asset: &AssetId,
    ) -> Result<Vec<RgbInventoryUtxo>, RgbError> {
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
        // the wallet's own UTXOs.
        let owned: std::collections::HashSet<(String, u32)> = wallet
            .wallet()
            .utxos()
            .map(|u| (u.outpoint.txid.to_string(), u.outpoint.vout.into_u32()))
            .collect();

        let mut utxos = Vec::new();
        let filter = FilterIncludeAll;
        for details in contract.schema.owned_types.values() {
            if let Ok(rgb_allocations) = contract.fungible(details.name.clone(), &filter) {
                for alloc in rgb_allocations {
                    let op = alloc.seal.to_outpoint();
                    let key = (op.txid.to_string(), op.vout.into_u32());
                    if !owned.contains(&key) {
                        continue;
                    }
                    utxos.push(RgbInventoryUtxo {
                        outpoint: Outpoint {
                            txid: key.0,
                            vout: key.1,
                        },
                        asset_id: asset.clone(),
                        amount: alloc.state.value(),
                        btc_sats: 0,
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
    ) -> Result<SwapTransfer, RgbError> {
        let account = self.load_signer()?;
        let mut wallet = self.load_wallet()?;

        // 1. Parse the taker's invoice and resolve the maker's payout address +
        //    reserved RGB inputs.
        let inputs = swap::prepare_buy_inputs(&mut wallet, rgb_invoice, maker_rgb_utxos)?;
       
        // 2. Assemble the unsigned swap tx (maker RGB + taker BTC inputs) with the
        //    commitment host; from here the witness txid is final.
        let mut psbt =
            swap::assemble_unsigned_psbt(&inputs, taker_btc_inputs, gross_btc_sats, actual_fee_sats)?;
       
        // 3. Build the RGB transition: pay the taker's seal, change back to the maker.
        let batch = swap::build_buy_transition(&wallet, &inputs, &psbt, amount)?;
       
        // 4. Embed + commit the transition and emit the witness-extended consignment.
        let (transfer, witness_id) =
            swap::commit_and_consign(&mut wallet, &mut psbt, batch, &inputs)?;
       
        // 5. Sign the maker's inputs (after commit — U4); taker BTC inputs stay open.
        swap::sign_maker_inputs(&mut psbt, &account)?;
       
        // 6. Encode the partial PSBT + consignment for the wire.
        swap::encode_swap_transfer(&psbt, &transfer, witness_id)
    }

    async fn finalize_after_taker_sign(
        &self,
        _signed_psbt_base64: &str,
        _original_consignment_base64: &str,
    ) -> Result<FinalizedSwap, RgbError> {
        // TODO(#13): finalize the PSBT with bp-std, extract the witness tx,
        // emit the witness-extended consignment.
        Err(RgbError::FinalizeFailed(
            "LibRgbBackend::finalize_after_taker_sign is not implemented yet (issue #13)"
                .to_owned(),
        ))
    }

    async fn create_invoice(&self, asset: &AssetId, amount: u64) -> Result<String, RgbError> {
        // Mirrors rgb-cmd's `Invoice` command for the blinded-seal / fungible
        // case: coin-select a seal-anchor outpoint (keychain 9), bind a fresh
        // graph seal to it, store it in the maker's stash, and emit an
        // `RgbInvoice` against the contract for `amount`. Real cryptographic
        // material — meaningful only with a live regtest/electrum stack.
        let mut wallet = self.load_wallet()?;
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;

        // Pick a UTXO on the RGB seal-anchor chain (BIP-389 keychain 9 — the
        // descriptor terminal is `/<0;1;9>/*`, with 0 = receive, 1 = change,
        // 9 = anchors). `Sats::ZERO` because the seal *references* the UTXO;
        // the invoice doesn't spend it. With ZERO the iterator yields every
        // eligible UTXO and we take the first.
        let outpoint = wallet
            .wallet()
            .coinselect(Sats::ZERO, |utxo| {
                RgbKeychain::contains_rgb(utxo.terminal.keychain)
            })
            .next()
            .ok_or_else(|| {
                RgbError::TransferBuild(
                    "no seal-anchor outpoint available; fund a keychain-9 address \
                     (see docs/regtest-rgb20-nia-dev-infra.md)"
                        .to_owned(),
                )
            })?;

        // Mint a fresh single-use seal bound to that outpoint. `new_random`
        // generates the blinding factor — the receiver-side secret that hides
        // which UTXO the invoice is for from anyone seeing the blinded form.
        // Stashing it via `store_secret_seal` is what lets us *open* the seal
        // later when accepting the sender's consignment.
        let network = wallet.wallet().network();
        let seal = GraphSeal::new_random(outpoint.txid, outpoint.vout);
        wallet
            .stock_mut()
            .store_secret_seal(seal)
            .map_err(|e| RgbError::TransferBuild(format!("store seal: {e}")))?;
        // Publish only the blinded commitment in the invoice — the sender
        // can't recover the underlying outpoint from this on its own.
        let beneficiary = Beneficiary::BlindedSeal(seal.to_secret_seal());

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

    async fn validate_incoming_consignment(
        &self,
        consignment_base64: &str,
        expected_invoice: &str,
    ) -> Result<ConsignmentInfo, RgbError> {
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

        // Cross-check the consignment is for the contract the invoice names.
        let invoice = RgbInvoice::from_str(expected_invoice)
            .map_err(|_| RgbError::InvalidInvoice)?;
      
        let invoice_contract = invoice.contract.ok_or_else(|| {
            RgbError::TransferBuild("expected_invoice carries no contract id".to_owned())
        })?;
       
        if consignment.contract_id() != invoice_contract {
            return Err(RgbError::TransferBuild(format!(
                "consignment contract {} does not match invoice contract {}",
                consignment.contract_id(),
                invoice_contract
            )));
        }

        // Cryptographic validation: resolver fetches witness txes; the typed
        // system from the maker's stash anchors the validator's trust root.
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
            .map_err(|e| RgbError::TransferBuild(format!("consignment validation: {e:?}")))?;
       
        if validated.validation_status().validity() != Validity::Valid {
            return Err(RgbError::TransferBuild(format!(
                "consignment invalid: {}",
                validated.validation_status()
            )));
        }

        // Introspect the validated consignment for owned-state allocations:
        // each terminal transition's fungible assignment carries a seal
        // (resolvable to a bitcoin outpoint) and an amount. Sum the amounts
        // and collect the outpoints; the swap-PSBT methods consume both.
        let mut outpoints = Vec::new();
        let mut total_amount: u64 = 0;
        // `ConsignmentApi::bundles_info` would be cleaner but isn't publicly
        // re-exported by rgb-api; the public `ConsignmentExt::bundled_witnesses`
        // exposes the same data via the `WitnessBundle` struct (pub fields).
        for wb in validated.bundled_witnesses() {
            let witness_txid = wb.pub_witness.txid();
            for known in &wb.bundle.known_transitions {
                for (_assignment_type, typed) in known.transition.assignments.iter() {
                    for assignment in typed.as_fungible() {
                        // `as_revealed_state` returns `&RevealedValue` (which
                        // wraps a `FungibleState`); reach the u64 via Wrapper.
                        let state = assignment.as_revealed_state();
                        total_amount =
                            total_amount.saturating_add(state.as_inner().as_u64());
                        if let Some(seal) = assignment.revealed_seal() {
                            // Witness-vout seals resolve against the bundle's
                            // anchoring txid; explicit seals carry their own.
                            let op = seal.outpoint_or(witness_txid);
                            outpoints.push(Outpoint {
                                txid: op.txid.to_string(),
                                vout: op.vout.into_u32(),
                            });
                        }
                    }
                }
            }
        }

        Ok(ConsignmentInfo {
            total_amount,
            outpoints,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_swap_psbt_sell(
        &self,
        _consignment_info: &ConsignmentInfo,
        _taker_rgb_prevouts: &[(Outpoint, TxOut)],
        _maker_btc_inputs: &[(Outpoint, TxOut)],
        _maker_rgb_invoice: &str,
        _btc_payout_addr: &str,
        _rgb_change_invoice: Option<&str>,
        _gross_btc_sats: u64,
        _actual_fee_sats: u64,
    ) -> Result<SwapTransfer, RgbError> {
        // TODO(#13): build the sell-side swap PSBT via bp-std + rgb-api and
        // sign the maker's BTC inputs.
        Err(RgbError::TransferBuild(
            "LibRgbBackend::create_swap_psbt_sell is not implemented yet (issue #13); \
             use MockRgbBackend for now"
                .to_owned(),
        ))
    }
}
