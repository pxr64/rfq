//! Self-bootstrapping regtest harness for the `LibRgbBackend` e2e tests.
//!
//! Replaces the env-var pattern (`RGB_DATA_DIR`, `RGB_CONTRACT_ID`, ...) the
//! old `#[ignore]`d tests used. The developer brings up bitcoind + electrs
//! once (`make -C infra/regtest regtest-up` + `make -C infra/regtest
//! rgb-tools-install`); this module discovers them and bootstraps everything
//! else from Rust: per-role wallets in a tempdir, funding via bitcoin-cli,
//! NIA contract issuance, and an issuer→maker transfer so the maker stash
//! has spendable RGB allocations.
//!
//! Shared via [`tokio::sync::OnceCell`] — first test to call [`stack`] pays
//! the bootstrap cost (~10s); subsequent tests reuse the same state for the
//! rest of the process lifetime. The tempdir auto-cleans on test-process
//! exit; the dev's docker stack is never touched.
//!
//! See GitHub issue #23 for the broader e2e harness plan and deferred work
//! (testcontainers, pure-Rust issuance, etc.).

#![allow(dead_code)] // not every helper is used by both pilot tests

use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use bpstd::psbt::Psbt;
use bpstd::signers::TestnetRefSigner;
use bpstd::{ConsensusEncode, HardenedIndex, XprivAccount, XpubDerivable};
use bpwallet::fs::FsTextStore;
use bpwallet::hot::{SecureIo, Seed, SeedType};
use bpwallet::{Bip43, Wallet as BpWallet};
use psrgbt::PsbtConstructor;
use rfq_types::{AssetId, AssetKind, BitcoinNetwork, Outpoint};
use rgb::RgbDescr;

// Was `use rfq_rgb::{...}` when this file lived in tests/common/mod.rs;
// it's now part of rfq-rgb's lib, so refer to siblings via `crate::`.
use crate::{ContractId, LibRgbBackend, RgbBackend, Taker, TxOut};
use tempfile::TempDir;
use tokio::sync::{Mutex, MutexGuard, OnceCell};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Bootstrap (on first call) and return a shared handle to the regtest stack.
/// Panics if the dev hasn't brought bitcoind + electrs up — bootstrap is not
/// allowed to mutate docker state.
pub async fn stack() -> &'static RegtestStack {
    STACK
        .get_or_try_init(|| async {
            tokio::task::spawn_blocking(RegtestStack::bootstrap)
                .await
                .map_err(|e| format!("bootstrap task join: {e}"))?
        })
        .await
        .expect("regtest harness bootstrap failed; see error message above")
}

static STACK: OnceCell<RegtestStack> = OnceCell::const_new();

pub struct RegtestStack {
    // Held for cleanup on drop; never accessed directly post-bootstrap.
    _tempdir: TempDir,
    tools_dir: PathBuf,
    compose_dir: PathBuf,
    electrum_url: String,
    contract_id: ContractId,
    contract_id_str: String,
    issuer: RoleHandles,
    maker: RoleHandles,
    /// Third-party role with its own RGB stash + funded BTC, used as the
    /// counterparty in the buy/sell round-trip tests. Drives the
    /// `taker_sign_and_finalize` half of the swap.
    taker: RoleHandles,
    /// Consignment bytes produced by the bootstrap issuer→maker transfer,
    /// cached for the validate test (and any future test that needs a real
    /// pre-accepted consignment).
    consignment_bytes: Vec<u8>,
    /// Maker's receive invoice from the bootstrap transfer (the same one
    /// the issuer paid). Cross-checked by `validate_incoming_consignment`.
    maker_invoice: String,
    /// Taker's receive invoice from the issuer→taker bootstrap transfer.
    /// The sell round-trip uses it to figure out which taker outpoint(s)
    /// carry RGB after the second transfer broadcasts and confirms.
    taker_invoice: String,
    /// All maker keychain-9 outpoints from the funding phase. The transfer
    /// landed RGB on one of them; the others are pure BTC and usable as
    /// `maker_btc_inputs` in the sell composition.
    maker_funding_outpoints: Vec<Outpoint>,
    /// Taker keychain-9 outpoints from the funding phase. Pure BTC (no RGB
    /// transfer ever lands on the taker in the bootstrap); the buy round-trip
    /// uses one as `taker_btc_inputs[0]`.
    taker_funding_outpoints: Vec<Outpoint>,
    /// Taker's keychain-0 address — the `btc_funding_addr` arg the maker
    /// passes to `create_swap_psbt_buy` so the taker's BTC change goes back
    /// to a taker-controlled address.
    taker_funding_addr: String,
    /// Issuer keychain-0 address; reused as a fresh-looking taker payout
    /// destination so the sell test doesn't need to know about the issuer.
    taker_payout_addr: String,
    /// Serializes every backend op so the autosave-on-drop semantics of
    /// `Stock::load(_, true)` don't race when multiple tests run in parallel.
    /// `cargo test -p rfq-rgb -- --ignored` works without `--test-threads=1`
    /// because of this lock; the lock is held for the lifetime of any
    /// [`MakerGuard`] / [`IssuerGuard`].
    backend_lock: Mutex<()>,
}

struct RoleHandles {
    stash_dir: PathBuf,
    account_file: PathBuf,
}

impl RegtestStack {
    pub fn asset(&self) -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: self.contract_id_str.clone(),
        }
    }

    pub fn contract_id(&self) -> ContractId {
        self.contract_id
    }

    pub fn contract_id_str(&self) -> &str {
        &self.contract_id_str
    }

    pub fn electrum_url(&self) -> &str {
        &self.electrum_url
    }

    /// Mint an ADDRESS-BASED (witness-vout / "future seal") invoice on the
    /// taker's stash via `rgb invoice -a`. The production `Taker::create_invoice`
    /// only emits witness-vout when no free keychain-9 anchor is available, which
    /// the funded harness never reproduces — this forces one so the maker's
    /// witness-vout swap composition can be exercised end-to-end.
    pub fn taker_witness_vout_invoice(&self, amount: u64) -> Result<String, String> {
        let out = rgb_cmd(
            &self.tools_dir,
            &self.taker.stash_dir,
            "taker",
            &self.electrum_url,
            &[
                "invoice",
                "-a",
                "--amount",
                &amount.to_string(),
                &self.contract_id_str,
            ],
        )?;
        parse_invoice(&out)
            .ok_or_else(|| format!("could not parse witness-vout invoice from:\n{out}"))
    }

    /// Acquire the shared backend lock and return a maker-side backend
    /// handle. Held until the guard drops — keeps `Stock::load` /
    /// autosave-on-drop from racing across parallel tests.
    pub async fn maker_backend(&self) -> MakerGuard<'_> {
        let guard = self.backend_lock.lock().await;
        MakerGuard {
            _guard: guard,
            backend: LibRgbBackend::new(
                self.maker.stash_dir.clone(),
                "maker".to_owned(),
                "regtest".to_owned(),
                self.electrum_url.clone(),
                self.maker.account_file.clone(),
                String::new(),
            ),
            stack: self,
        }
    }

    /// Acquire the shared backend lock and return an issuer-side handle.
    pub async fn issuer_backend(&self) -> IssuerGuard<'_> {
        let guard = self.backend_lock.lock().await;
        IssuerGuard {
            _guard: guard,
            backend: LibRgbBackend::new(
                self.issuer.stash_dir.clone(),
                "issuer".to_owned(),
                "regtest".to_owned(),
                self.electrum_url.clone(),
                self.issuer.account_file.clone(),
                String::new(),
            ),
        }
    }

    /// Acquire the shared backend lock and return a taker-side handle.
    /// The buy/sell round-trip tests use this to read taker UTXOs and to
    /// sign+finalize the maker-built PSBT before handing it back.
    pub async fn taker_backend(&self) -> TakerGuard<'_> {
        let guard = self.backend_lock.lock().await;
        TakerGuard {
            _guard: guard,
            stash_dir: self.taker.stash_dir.clone(),
            account_file: self.taker.account_file.clone(),
            electrum_url: self.electrum_url.clone(),
            funding_outpoints: self.taker_funding_outpoints.clone(),
        }
    }

    pub fn consignment_bytes(&self) -> &[u8] {
        &self.consignment_bytes
    }

    pub fn maker_invoice(&self) -> &str {
        &self.maker_invoice
    }

    pub fn taker_invoice(&self) -> &str {
        &self.taker_invoice
    }

    /// Run `rgb transfer` from the taker's stash for `recipient_invoice`,
    /// returning the raw consignment bytes. The PSBT `rgb transfer`
    /// produces is discarded — the swap composition builds its own. Used
    /// by the sell round-trip to manufacture a taker→maker consignment
    /// for `validate_incoming_consignment` + `create_swap_psbt_sell`.
    ///
    /// Mutates the taker's stash (the transition is recorded as if the
    /// throwaway PSBT had been broadcast). Subsequent transfers from the
    /// taker would see the RGB as already consigned — fine for the
    /// single-test-run shape, since `RegtestStack` is bootstrapped once
    /// per process.
    pub fn taker_consignment_for(&self, recipient_invoice: &str) -> Result<Vec<u8>, String> {
        let label = format!("taker-{}", uniq_label());
        let consignment_path = self
            ._tempdir
            .path()
            .join(format!("{label}.consignment.rgb"));
        let psbt_path = self._tempdir.path().join(format!("{label}.psbt"));
        rgb_cmd(
            &self.tools_dir,
            &self.taker.stash_dir,
            "taker",
            &self.electrum_url,
            &[
                "transfer",
                recipient_invoice,
                &consignment_path.display().to_string(),
                &psbt_path.display().to_string(),
            ],
        )?;
        std::fs::read(&consignment_path)
            .map_err(|e| format!("read taker consignment {consignment_path:?}: {e}"))
    }

    pub fn taker_funding_addr(&self) -> &str {
        &self.taker_funding_addr
    }

    pub fn taker_payout_addr(&self) -> &str {
        &self.taker_payout_addr
    }

    pub fn compose_dir(&self) -> &Path {
        &self.compose_dir
    }

    /// Filesystem paths the dependent-crate integration tests need to
    /// hand to `maker_node::RgbConfig` (so they can build a runtime
    /// pointing at the same maker stash the harness already
    /// bootstrapped). Exposed as accessors rather than `pub` fields
    /// to keep `RoleHandles` itself encapsulated.
    pub fn maker_stash_dir(&self) -> &Path {
        &self.maker.stash_dir
    }

    pub fn maker_account_file(&self) -> &Path {
        &self.maker.account_file
    }

    /// Broadcast a raw witness tx via `bitcoin-cli sendrawtransaction`.
    /// Default `maxfeerate` is fine post-issue-#25: the swap composition
    /// now routes seal-anchor BTC value back to a maker change output
    /// (buy) / folds it into the taker payout (sell), so the tx fee is
    /// bounded by `actual_fee_sats` rather than the seal-anchor value.
    pub fn broadcast(&self, raw_tx: &[u8]) -> Result<String, String> {
        let hex = raw_tx
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let out = bitcoin_cli(&self.compose_dir, &["sendrawtransaction", &hex])?;
        Ok(out.trim().to_owned())
    }

    /// Send `btc` BTC from the miner wallet to `addr`, mine a
    /// confirmation block, and wait for electrs to index it. Used by
    /// tests that need a specific address pre-funded — e.g. the
    /// maker-node HTTP test's `btc_funding_addr` (keychain 0), which
    /// `fund_role` never touches.
    pub fn fund_address(&self, addr: &str, btc: &str) -> Result<(), String> {
        bitcoin_cli(
            &self.compose_dir,
            &["-rpcwallet=miner", "sendtoaddress", addr, btc],
        )?;
        self.mine_block()?;
        wait_for_electrs_tip(&self.compose_dir, &self.electrum_url)?;
        Ok(())
    }

    /// Mine one block to the miner wallet. Used by tests that need a
    /// just-broadcast witness tx to confirm so a follow-up swap can spend
    /// its outputs (e.g. issue #27's chain-observer two-buy verification).
    pub fn mine_block(&self) -> Result<(), String> {
        let miner_addr_out = bitcoin_cli(
            &self.compose_dir,
            &["-rpcwallet=miner", "getnewaddress", "", "bech32"],
        )?;
        let miner_addr = miner_addr_out.trim();
        bitcoin_cli(
            &self.compose_dir,
            &["-rpcwallet=miner", "generatetoaddress", "1", miner_addr],
        )?;
        Ok(())
    }
}

/// Maker backend handle holding the shared backend lock. Derefs to
/// [`LibRgbBackend`] so existing call sites keep working.
pub struct MakerGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    backend: LibRgbBackend,
    stack: &'a RegtestStack,
}

impl<'a> std::ops::Deref for MakerGuard<'a> {
    type Target = LibRgbBackend;
    fn deref(&self) -> &Self::Target {
        &self.backend
    }
}

impl<'a> MakerGuard<'a> {
    /// A maker-owned BTC outpoint that doesn't carry an RGB allocation —
    /// usable as `maker_btc_inputs[0]` in a sell-side swap PSBT. Uses the
    /// guard's already-locked backend, so no extra lock acquisition.
    pub async fn spare_btc_outpoint(&self) -> Outpoint {
        let rgb_utxos = self
            .backend
            .list_inventory_utxos(&self.stack.asset())
            .await
            .expect("list_inventory_utxos for spare-outpoint lookup");
        let rgb_set: std::collections::HashSet<&Outpoint> =
            rgb_utxos.iter().map(|u| &u.outpoint).collect();
        self.stack
            .maker_funding_outpoints
            .iter()
            .find(|op| !rgb_set.contains(op))
            .cloned()
            .expect(
                "no spare maker BTC outpoint — bump the funding count in \
                 bootstrap if that ever happens",
            )
    }
}

/// Issuer backend handle holding the shared backend lock.
pub struct IssuerGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    backend: LibRgbBackend,
}

impl<'a> std::ops::Deref for IssuerGuard<'a> {
    type Target = LibRgbBackend;
    fn deref(&self) -> &Self::Target {
        &self.backend
    }
}

/// Taker-side handle holding the shared backend lock. There's no maker-side
/// `LibRgbBackend` here — the taker only needs to read its own UTXOs and
/// drive `psbt.sign` + `psbt.finalize` on the maker-built PSBT, so the
/// guard exposes those operations directly via bp-std / bp-wallet calls.
///
/// Keeping this out of `LibRgbBackend` mirrors the architectural split: the
/// `RgbBackend` trait is the maker-side abstraction; the taker is just a
/// counterparty that owns BTC inputs and applies signatures.
pub struct TakerGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    stash_dir: PathBuf,
    account_file: PathBuf,
    electrum_url: String,
    funding_outpoints: Vec<Outpoint>,
}

impl<'a> TakerGuard<'a> {
    /// Read the taker's RGB inventory for `asset` via an internal
    /// `LibRgbBackend`. `LibRgbBackend`'s read paths are role-agnostic;
    /// the trait's *write* path (the swap-PSBT trio) is what's
    /// conceptually maker-only and never called here. Used by the sell
    /// round-trip to find which outpoint the bootstrap issuer→taker
    /// transfer landed RGB on.
    pub async fn inventory(
        &self,
        asset: &rfq_types::AssetId,
    ) -> Result<Vec<rfq_types::RgbInventoryUtxo>, String> {
        self.taker()
            .inventory(asset)
            .await
            .map_err(|e| e.to_string())
    }

    /// Mint a fresh RGB invoice on the taker's stash. The sell round-trip
    /// uses this to produce an `rgb_change_invoice` for over-consigned
    /// surplus (taker sends a consignment that drains 500 RGB but the
    /// swap delivers only 200 — the remaining 300 routes back to the
    /// taker via this invoice's blinded seal).
    pub async fn create_invoice(
        &self,
        asset: &rfq_types::AssetId,
        amount: u64,
    ) -> Result<String, String> {
        self.taker()
            .create_invoice(asset, amount)
            .await
            .map_err(|e| e.to_string())
    }

    /// Refresh the taker's bp-wallet UTXO cache against electrum so
    /// post-broadcast BTC change outputs (e.g. from a prior buy swap)
    /// become visible to `spare_btc_input` / `lookup_prevout`. Mirrors
    /// `LibRgbBackend::sync_wallet` on the maker side.
    pub async fn sync_wallet(&self) -> Result<(), String> {
        self.taker()
            .sync_wallet()
            .await
            .map_err(|e| e.to_string())
    }

    /// The production [`Taker`] backing this guard. Every op delegates here;
    /// the guard only adds the shared-backend mutex (held in `_guard`).
    fn taker(&self) -> Taker {
        Taker::new(
            self.stash_dir.clone(),
            "taker".to_owned(),
            "regtest".to_owned(),
            self.electrum_url.clone(),
            self.account_file.clone(),
            String::new(),
        )
    }

    /// Build the taker's sell consignment in-process (unilateral RGB transfer
    /// to the maker's invoice) — the counterpart to
    /// `RegtestStack::taker_consignment_for`'s `rgb transfer` shell-out.
    pub async fn create_transfer_to_invoice(
        &self,
        recipient_invoice: &str,
        fee_sats: u64,
    ) -> Result<String, String> {
        self.taker()
            .create_transfer_to_invoice(recipient_invoice, fee_sats)
            .await
            .map_err(|e| e.to_string())
    }

    /// Look up a specific taker UTXO and return its (Outpoint, TxOut). Used
    /// by the sell round-trip to build `taker_rgb_prevouts` from the
    /// outpoints the validated consignment reports.
    pub fn lookup_prevout(&self, want: &Outpoint) -> Result<(Outpoint, TxOut), String> {
        self.taker().lookup_prevout(want).map_err(|e| e.to_string())
    }

    /// Any taker-controlled UTXO that does **not** carry an RGB allocation
    /// for `asset`, paired with its prevout `TxOut`. Used as
    /// `taker_btc_inputs[0]` in `create_swap_psbt_buy` — must skip
    /// RGB-bearing outpoints, otherwise the swap tx would consume the
    /// taker's RGB allocation at bitcoin layer without an RGB transition
    /// accounting for it.
    ///
    /// Scans the full bp-wallet UTXO cache (not just the bootstrap
    /// keychain-9 funding outpoints), so post-broadcast BTC change from
    /// a prior swap — landing on keychain 1 (the change chain) — is also
    /// eligible. Required for consecutive-swap tests (issue #27/#28).
    pub async fn spare_btc_input(
        &self,
        asset: &rfq_types::AssetId,
    ) -> Result<(Outpoint, TxOut), String> {
        self.taker()
            .spare_btc_input(asset)
            .await
            .map_err(|e| e.to_string())
    }

    /// Enrich every PSBT input whose `prev_output` belongs to the taker
    /// wallet, then sign + finalize against the taker descriptor. Mirrors
    /// what an external taker would do in its own wallet after receiving
    /// the maker-built partial PSBT.
    ///
    /// Enrichment is mandatory: the buy/sell composition leaves taker
    /// inputs with only `witness_utxo` + `sighash_type` (the maker doesn't
    /// know the taker's descriptor), but `TestnetRefSigner` keys off
    /// `bip32_derivation` to pick a key — without it, `psbt.sign` returns
    /// 0 matched inputs. `crate::enrich_psbt_input` populates the
    /// missing fields the same way the maker side enriches its own.
    ///
    /// Inputs the taker doesn't own (the maker's RGB inputs, already
    /// carrying `partial_sigs`) are left untouched for the maker's own
    /// finalize step.
    pub fn sign_and_finalize(&self, partial_psbt_b64: &str) -> Result<String, String> {
        self.taker()
            .sign_and_finalize(partial_psbt_b64)
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

impl RegtestStack {
    fn bootstrap() -> Result<RegtestStack, String> {
        let workspace_root = workspace_root();
        let tools_dir = env_or_default(
            "RGB_RFQ_TOOLS_DIR",
            workspace_root.join("infra/regtest/tools"),
        );

        let compose_dir =
            env_or_default("RGB_RFQ_COMPOSE_DIR", workspace_root.join("infra/regtest"));

        let electrum_url =
            std::env::var("ELECTRUM_URL").unwrap_or_else(|_| "localhost:50001".to_owned());

        require_tools(&tools_dir)?;
        require_stack_up(&compose_dir, &electrum_url)?;

        let tempdir = TempDir::new().map_err(|e| format!("tempdir: {e}"))?;
        let schema_file =
            workspace_root.join("crates/rfq-rgb/tests/fixtures/NonInflatableAsset.rgb");
        // Git-tracked template (regtest-reset wipes artifacts/); falls back to a
        // legacy artifacts copy if one still exists locally.
        let template_path = {
            let tracked = workspace_root.join("infra/regtest/contracts/rfq-nia.yaml");
            let legacy = workspace_root.join("infra/regtest/artifacts/rfq-nia.yaml");
            if tracked.exists() {
                tracked
            } else {
                legacy
            }
        };

        // Phase 1: per-role wallet creation (issuer + maker + taker).
        let issuer = create_role_wallet(&tools_dir, tempdir.path(), &electrum_url, "issuer")?;
        let maker = create_role_wallet(&tools_dir, tempdir.path(), &electrum_url, "maker")?;
        let taker = create_role_wallet(&tools_dir, tempdir.path(), &electrum_url, "taker")?;

        // Phase 2: import the vendored NIA schema into each stash.
        import_schema(
            &tools_dir,
            &issuer.stash_dir,
            "issuer",
            &electrum_url,
            &schema_file,
        )?;
        import_schema(
            &tools_dir,
            &maker.stash_dir,
            "maker",
            &electrum_url,
            &schema_file,
        )?;
        import_schema(
            &tools_dir,
            &taker.stash_dir,
            "taker",
            &electrum_url,
            &schema_file,
        )?;

        // Phase 3: fund each role on a keychain-9 address. Issuer gets two:
        // one is consumed as the genesis seal + spent by the first transfer,
        // the second provides BTC change for the second transfer's anchor.
        // Maker gets two so the sell test has a spare BTC UTXO after the
        // issuer→maker transfer claims one for RGB. Taker also gets two —
        // the issuer→taker bootstrap transfer commits RGB to one taker
        // outpoint (via the taker's blinded seal), so the buy round-trip
        // needs a *spare* (non-RGB) outpoint as its BTC input to avoid
        // spending Y0's RGB allocation at bitcoin layer without an RGB
        // transition accounting for it.
        fund_role(
            &tools_dir,
            &compose_dir,
            &issuer.stash_dir,
            "issuer",
            &electrum_url,
        )?;
        fund_role(
            &tools_dir,
            &compose_dir,
            &issuer.stash_dir,
            "issuer",
            &electrum_url,
        )?;
        fund_role(
            &tools_dir,
            &compose_dir,
            &maker.stash_dir,
            "maker",
            &electrum_url,
        )?;
        fund_role(
            &tools_dir,
            &compose_dir,
            &maker.stash_dir,
            "maker",
            &electrum_url,
        )?;
        fund_role(
            &tools_dir,
            &compose_dir,
            &taker.stash_dir,
            "taker",
            &electrum_url,
        )?;
        fund_role(
            &tools_dir,
            &compose_dir,
            &taker.stash_dir,
            "taker",
            &electrum_url,
        )?;

        // Phase 4: issue the NIA contract.
        let contract_id_str = issue_contract(
            &tools_dir,
            &issuer.stash_dir,
            "issuer",
            &electrum_url,
            &template_path,
            tempdir.path(),
        )?;
        let contract_id = ContractId::from_str(&contract_id_str)
            .map_err(|e| format!("parse contract id `{contract_id_str}`: {e}"))?;

        // Phase 5a: issuer→maker transfer so the maker has RGB allocations
        // to list / spend. Cache the consignment + invoice so the validate
        // / sell tests don't need their own transfer. Broadcasted so the
        // issuer's RGB change lands at a real outpoint for phase 5b.
        let maker_artifacts = transfer_rgb(
            &tools_dir,
            &compose_dir,
            &issuer.stash_dir,
            "issuer",
            &issuer.account_file,
            TransferRecipient {
                stash: &maker.stash_dir,
                role: "maker",
            },
            &electrum_url,
            &contract_id_str,
            1000,
            tempdir.path(),
            "issuer-to-maker",
        )?;

        // Phase 5b: issuer→taker transfer so the taker has RGB to consign
        // in the sell round-trip test. Requires phase 5a to have broadcast
        // (the issuer's RGB change from 5a is what 5b spends).
        let taker_artifacts = transfer_rgb(
            &tools_dir,
            &compose_dir,
            &issuer.stash_dir,
            "issuer",
            &issuer.account_file,
            TransferRecipient {
                stash: &taker.stash_dir,
                role: "taker",
            },
            &electrum_url,
            &contract_id_str,
            500,
            tempdir.path(),
            "issuer-to-taker",
        )?;

        // Phase 6: collect post-bootstrap state the tests consume.
        let maker_utxos_out = rgb_cmd(
            &tools_dir,
            &maker.stash_dir,
            "maker",
            &electrum_url,
            &["utxos"],
        )?;
        let maker_funding_outpoints = parse_all_keychain9_outpoints(&maker_utxos_out);
        if maker_funding_outpoints.len() < 2 {
            return Err(format!(
                "expected ≥2 maker keychain-9 outpoints after funding, got {}:\n{maker_utxos_out}",
                maker_funding_outpoints.len()
            ));
        }
        let issuer_addr_out = rgb_cmd(
            &tools_dir,
            &issuer.stash_dir,
            "issuer",
            &electrum_url,
            &["address", "-k", "0"],
        )?;
        let taker_payout_addr = last_word(&issuer_addr_out)
            .ok_or_else(|| format!("could not parse taker payout addr:\n{issuer_addr_out}"))?;

        let taker_utxos_out = rgb_cmd(
            &tools_dir,
            &taker.stash_dir,
            "taker",
            &electrum_url,
            &["utxos"],
        )?;
        let taker_funding_outpoints = parse_all_keychain9_outpoints(&taker_utxos_out);
        if taker_funding_outpoints.len() < 2 {
            return Err(format!(
                "expected ≥2 taker keychain-9 outpoints after funding, got {}:\n{taker_utxos_out}",
                taker_funding_outpoints.len()
            ));
        }
        let taker_funding_addr_out = rgb_cmd(
            &tools_dir,
            &taker.stash_dir,
            "taker",
            &electrum_url,
            &["address", "-k", "0"],
        )?;
        let taker_funding_addr = last_word(&taker_funding_addr_out).ok_or_else(|| {
            format!("could not parse taker funding addr:\n{taker_funding_addr_out}")
        })?;

        Ok(RegtestStack {
            _tempdir: tempdir,
            tools_dir,
            compose_dir,
            electrum_url,
            contract_id,
            contract_id_str,
            issuer,
            maker,
            taker,
            consignment_bytes: maker_artifacts.consignment_bytes,
            maker_invoice: maker_artifacts.maker_invoice,
            taker_invoice: taker_artifacts.maker_invoice,
            maker_funding_outpoints,
            taker_funding_outpoints,
            taker_funding_addr,
            taker_payout_addr,
            backend_lock: Mutex::new(()),
        })
    }
}

/// Captured during a bootstrap RGB transfer for later test use. `maker_invoice`
/// is the recipient's invoice (named for historical reasons; reused for both
/// issuer→maker and issuer→taker transfers).
struct TransferArtifacts {
    consignment_bytes: Vec<u8>,
    maker_invoice: String,
}

// ---------------------------------------------------------------------------
// Bootstrap phases (mirror infra/regtest/scripts/*)
// ---------------------------------------------------------------------------

fn create_role_wallet(
    tools_dir: &Path,
    tempdir: &Path,
    electrum_url: &str,
    role: &str,
) -> Result<RoleHandles, String> {
    let account_file = tempdir.join(format!("{role}.account"));
    let stash_dir = tempdir.join(role);
    std::fs::create_dir_all(&stash_dir)
        .map_err(|e| format!("create stash dir {stash_dir:?}: {e}"))?;

    // In-Rust replacement for `bp-hot seed` + `bp-hot derive -N --scheme bip84
    // --account 0h ...`. The seed itself stays in memory — `LibRgbBackend`
    // only needs the encrypted account file (loaded via `XprivAccount::read`
    // in `lib_backend.rs:122`). `derive(_, testnet=true, _)` mirrors the
    // shell's omission of `--mainnet`; the empty account password mirrors the
    // shell's `-N` (`--no-password`) flag.
    let seed = Seed::random(SeedType::Bit128);
    let account = seed.derive(Bip43::Bip84, true, HardenedIndex::hardened(0));
    account
        .write(&account_file, "")
        .map_err(|e| format!("write account file {account_file:?}: {e}"))?;
    // `to_xpub_account().to_string()` mirrors the `Account:` line bp-hot
    // derive used to print: `[fingerprint/84h/1h/0h]tpubD...`. The
    // `/<0;1;9>/*` terminal declares external (0), change (1), and RGB
    // seal-anchor (9) keychains — `rgb address -k 9` needs keychain 9 in
    // the descriptor or it can't derive anchor addresses.
    let descriptor = account.to_xpub_account().to_string();
    let descriptor_with_terminal = format!("{descriptor}/<0;1;9>/*");

    // rgb create --wpkh <descriptor> <role>
    rgb_cmd(
        tools_dir,
        &stash_dir,
        role,
        electrum_url,
        &["create", "--wpkh", &descriptor_with_terminal, role],
    )?;

    Ok(RoleHandles {
        stash_dir,
        account_file,
    })
}

fn import_schema(
    tools_dir: &Path,
    stash_dir: &Path,
    role: &str,
    electrum_url: &str,
    schema_file: &Path,
) -> Result<(), String> {
    rgb_cmd(
        tools_dir,
        stash_dir,
        role,
        electrum_url,
        &["import", &schema_file.display().to_string()],
    )?;
    Ok(())
}

fn fund_role(
    tools_dir: &Path,
    compose_dir: &Path,
    stash_dir: &Path,
    role: &str,
    electrum_url: &str,
) -> Result<(), String> {
    // Derive a keychain-9 address.
    let addr_out = rgb_cmd(
        tools_dir,
        stash_dir,
        role,
        electrum_url,
        &["address", "-k", "9"],
    )?;
    let addr = last_word(&addr_out).ok_or_else(|| format!("could not parse address for {role}"))?;

    ensure_miner_wallet(compose_dir)?;
    bitcoin_cli(
        compose_dir,
        &["-rpcwallet=miner", "sendtoaddress", &addr, "1"],
    )?;
    let miner_addr_out = bitcoin_cli(
        compose_dir,
        &["-rpcwallet=miner", "getnewaddress", "", "bech32"],
    )?;
    let miner_addr = miner_addr_out.trim();
    bitcoin_cli(
        compose_dir,
        &["-rpcwallet=miner", "generatetoaddress", "1", miner_addr],
    )?;

    // Wait for electrs to index the just-mined block before we ask
    // bp-wallet (via rgb-cmd `utxos --sync`) to scan. Without this, the
    // sync queries electrs while it's still indexing and returns an
    // empty UTXO set — manifests as "wallet total balance: 0 ṩ" at
    // bootstrap time.
    wait_for_electrs_tip(compose_dir, electrum_url)?;

    // Sync so electrs's new UTXO shows up in the stash.
    rgb_cmd(
        tools_dir,
        stash_dir,
        role,
        electrum_url,
        &["utxos", "--sync"],
    )?;
    Ok(())
}

fn issue_contract(
    tools_dir: &Path,
    issuer_stash: &Path,
    issuer_role: &str,
    electrum_url: &str,
    template_path: &Path,
    tempdir: &Path,
) -> Result<String, String> {
    // Schema id is content-addressed; read it back from the stash rather
    // than hard-coding so updates to the schema fixture flow through.
    let schemata_out = rgb_cmd(
        tools_dir,
        issuer_stash,
        issuer_role,
        electrum_url,
        &["schemata"],
    )?;
    let schema_id = parse_schema_id(&schemata_out)
        .ok_or_else(|| format!("could not parse schema id from:\n{schemata_out}"))?;

    let utxos_out = rgb_cmd(
        tools_dir,
        issuer_stash,
        issuer_role,
        electrum_url,
        &["utxos"],
    )?;
    let outpoint = parse_keychain9_outpoint(&utxos_out)
        .ok_or_else(|| format!("could not find a keychain-9 outpoint in:\n{utxos_out}"))?;

    let template = std::fs::read_to_string(template_path)
        .map_err(|e| format!("read template {template_path:?}: {e}"))?;
    let rendered = render_yaml_template(&template, &schema_id, &outpoint);
    let rendered_path = tempdir.join("rfq-nia.rendered.yaml");
    std::fs::write(&rendered_path, &rendered).map_err(|e| format!("write rendered yaml: {e}"))?;

    // `rgb issue` puts the contract id on stderr — read both streams.
    let mut cmd = Command::new(rgb_path(tools_dir));
    cmd.arg("-n")
        .arg("regtest")
        .arg(format!("--electrum={electrum_url}"))
        .arg("-d")
        .arg(issuer_stash)
        .arg("-w")
        .arg(issuer_role)
        .arg("issue")
        .arg("ssi:issuer")
        .arg(&rendered_path);
    let (issue_stdout, issue_stderr) = run_split("rgb issue (issuer)", &mut cmd)?;
    let issue_combined = format!("{issue_stdout}\n{issue_stderr}");

    if let Some(id) = parse_contract_id(&issue_combined) {
        return Ok(id);
    }
    // Fallback: scan `rgb contracts` for the freshly issued id, same as
    // rgb-issue-asset:99-102.
    let contracts_out = rgb_cmd(
        tools_dir,
        issuer_stash,
        issuer_role,
        electrum_url,
        &["contracts"],
    )?;
    parse_contract_id(&contracts_out).ok_or_else(|| {
        format!(
            "could not parse contract id from `rgb issue`:\nstdout:\n{issue_stdout}\nstderr:\n{issue_stderr}\n\
             nor from `rgb contracts`:\n{contracts_out}"
        )
    })
}

/// Recipient-side context for [`transfer_rgb`]. Bundles the path layout so
/// the sender can resolve the recipient's wallet name for invoice/accept
/// calls without the caller having to pass everything separately.
struct TransferRecipient<'a> {
    stash: &'a Path,
    role: &'a str,
}

/// Issuer-style RGB transfer that signs + broadcasts the witness tx and
/// confirms it (vs. the earlier stash-only flow). Concretely:
///   1. Recipient creates an invoice for `amount`.
///   2. Sender runs `rgb transfer` (rgb-api `pay`) → consignment + PSBT.
///   3. Sender's PSBT is signed + finalized in-Rust via the sender's
///      XprivAccount, then extracted and broadcast through bitcoind.
///   4. A block is mined to confirm; both sides `utxos --sync` so the
///      bp-wallet caches reflect the new on-chain state (sender's RGB
///      change is now at a *real* outpoint, ready for the next transfer).
///   5. Recipient accepts the consignment into its stash.
///
/// Broadcasting is essential for chained transfers: a stash-only transfer
/// leaves the sender's RGB change at a tentative outpoint that
/// `WalletUnspentFilter` excludes from the next `pay` call, so the second
/// transfer would fail with "no allocations".
fn transfer_rgb(
    tools_dir: &Path,
    compose_dir: &Path,
    sender_stash: &Path,
    sender_role: &str,
    sender_account_file: &Path,
    recipient: TransferRecipient<'_>,
    electrum_url: &str,
    contract_id: &str,
    amount: u64,
    tempdir: &Path,
    label: &str,
) -> Result<TransferArtifacts, String> {
    let invoice_out = rgb_cmd(
        tools_dir,
        recipient.stash,
        recipient.role,
        electrum_url,
        &["invoice", "--amount", &amount.to_string(), contract_id],
    )?;
    let invoice = parse_invoice(&invoice_out)
        .ok_or_else(|| format!("could not parse invoice from:\n{invoice_out}"))?;

    let consignment_path = tempdir.join(format!("{label}.consignment.rgb"));
    let psbt_path = tempdir.join(format!("{label}.psbt"));
    rgb_cmd(
        tools_dir,
        sender_stash,
        sender_role,
        electrum_url,
        &[
            "transfer",
            &invoice,
            &consignment_path.display().to_string(),
            &psbt_path.display().to_string(),
        ],
    )?;

    // Sign + finalize + broadcast the sender's witness tx.
    sign_finalize_and_broadcast(
        sender_stash,
        sender_role,
        sender_account_file,
        &psbt_path,
        compose_dir,
    )?;

    // Confirm and sync both sides so the witness tx + spent/created UTXOs
    // are reflected in each wallet's cache.
    let miner_addr_out = bitcoin_cli(
        compose_dir,
        &["-rpcwallet=miner", "getnewaddress", "", "bech32"],
    )?;
    let miner_addr = miner_addr_out.trim();
    bitcoin_cli(
        compose_dir,
        &["-rpcwallet=miner", "generatetoaddress", "1", miner_addr],
    )?;
    rgb_cmd(
        tools_dir,
        sender_stash,
        sender_role,
        electrum_url,
        &["utxos", "--sync"],
    )?;
    rgb_cmd(
        tools_dir,
        recipient.stash,
        recipient.role,
        electrum_url,
        &["utxos", "--sync"],
    )?;

    rgb_cmd(
        tools_dir,
        recipient.stash,
        recipient.role,
        electrum_url,
        &["accept", &consignment_path.display().to_string()],
    )?;

    let consignment_bytes = std::fs::read(&consignment_path)
        .map_err(|e| format!("read consignment {consignment_path:?}: {e}"))?;
    Ok(TransferArtifacts {
        consignment_bytes,
        maker_invoice: invoice,
    })
}

/// Load `psbt_path`, sign every input the role's account can satisfy,
/// finalize against the role's wallet descriptor, extract the witness tx,
/// and broadcast it via `bitcoin-cli sendrawtransaction`. Used by
/// [`transfer_rgb`] to confirm chained transfers on-chain.
///
/// The rgb-api `pay` flow already enriches every input with full
/// `bip32_derivation` (it built the PSBT against the sender's own
/// descriptor), so no extra enrichment is needed — `psbt.sign` matches
/// straight away.
fn sign_finalize_and_broadcast(
    stash_dir: &Path,
    wallet_name: &str,
    account_file: &Path,
    psbt_path: &Path,
    compose_dir: &Path,
) -> Result<String, String> {
    let provider = FsTextStore::new(stash_dir.join("regtest").join(wallet_name))
        .map_err(|e| format!("{wallet_name} wallet provider: {e}"))?;
    let wallet: BpWallet<XpubDerivable, RgbDescr> =
        BpWallet::load(provider, true).map_err(|e| format!("{wallet_name} wallet load: {e}"))?;
    let descriptor = wallet.descriptor().clone();

    let account = XprivAccount::read(account_file, "")
        .map_err(|e| format!("{wallet_name} account read: {e}"))?;
    let psbt_bytes =
        std::fs::read(psbt_path).map_err(|e| format!("read PSBT {psbt_path:?}: {e}"))?;
    let mut psbt = Psbt::deserialize(&psbt_bytes)
        .map_err(|e| format!("deserialize PSBT {psbt_path:?}: {e}"))?;

    let signer = TestnetRefSigner::new(&account);
    let sig_count = psbt
        .sign(&signer)
        .map_err(|e| format!("{wallet_name} sign: {e}"))?;
    if sig_count == 0 {
        return Err(format!("{wallet_name} signer matched no inputs"));
    }
    psbt.finalize(&descriptor);
    if !psbt.is_finalized() {
        return Err(format!("PSBT not finalized after {wallet_name} finalize"));
    }
    let tx = psbt
        .extract()
        .map_err(|e| format!("extract witness tx: {e}"))?;
    let hex = tx
        .consensus_serialize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let out = bitcoin_cli(compose_dir, &["sendrawtransaction", &hex])?;
    Ok(out.trim().to_owned())
}

// ---------------------------------------------------------------------------
// Subprocess wrappers
// ---------------------------------------------------------------------------

fn rgb_cmd(
    tools_dir: &Path,
    stash_dir: &Path,
    wallet_name: &str,
    electrum_url: &str,
    args: &[&str],
) -> Result<String, String> {
    // rgb-cmd 0.11.1-rc.6 wants the joined `--electrum=URL` form; the
    // space-separated form makes clap treat URL as a subcommand. The shell
    // wrappers in common.sh use the joined form for the same reason.
    let mut cmd = Command::new(rgb_path(tools_dir));
    cmd.arg("-n")
        .arg("regtest")
        .arg(format!("--electrum={electrum_url}"))
        .arg("-d")
        .arg(stash_dir)
        .arg("-w")
        .arg(wallet_name)
        .args(args);
    run(
        &format!(
            "rgb {} ({wallet_name})",
            args.first().copied().unwrap_or("")
        ),
        &mut cmd,
    )
}

fn bitcoin_cli(compose_dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("docker");
    cmd.current_dir(compose_dir).args([
        "compose",
        "exec",
        "-T",
        "bitcoind",
        "bitcoin-cli",
        "-regtest",
        "-datadir=/home/bitcoin/.bitcoin",
    ]);
    cmd.args(args);
    run(
        &format!("bitcoin-cli {}", args.first().copied().unwrap_or("")),
        &mut cmd,
    )
}

fn run(label: &str, cmd: &mut Command) -> Result<String, String> {
    let (stdout, _stderr) = run_split(label, cmd)?;
    Ok(stdout)
}

/// Variant of `run` that returns stdout and stderr separately — needed for
/// `rgb issue`, which emits the contract id on stderr. Most callers want
/// stdout-only (stderr typically holds "Loading descriptor..." noise that
/// would confuse last-word / token parsers).
fn run_split(label: &str, cmd: &mut Command) -> Result<(String, String), String> {
    let output = cmd
        .output()
        .map_err(|e| format!("{label}: failed to spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{label}: exit {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// Block until electrs has indexed the same tip bitcoind reports. The
/// bootstrap's `rgb utxos --sync` query goes through bp-electrum to
/// electrs; if electrs is still indexing the just-mined block at sync
/// time, the query returns an empty UTXO set and the wallet thinks it
/// has zero balance. Poll the electrum tip in a tight loop (≤2s
/// budget); regtest indexing is sub-second under normal load.
fn wait_for_electrs_tip(compose_dir: &Path, electrum_url: &str) -> Result<(), String> {
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    let target: u64 = bitcoin_cli(compose_dir, &["getblockcount"])?
        .trim()
        .parse()
        .map_err(|e| format!("parse getblockcount: {e}"))?;
    let client = electrum::Client::new(electrum_url)
        .map_err(|e| format!("electrum connect for sync wait: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        use electrum::ElectrumApi as _;
        let header = client
            .block_headers_subscribe_raw()
            .map_err(|e| format!("electrum headers_subscribe: {e}"))?;
        if header.height as u64 >= target {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "electrs tip {} did not catch up to bitcoind tip {} within 2s",
                header.height, target
            ));
        }
        sleep(Duration::from_millis(50));
    }
}

fn ensure_miner_wallet(compose_dir: &Path) -> Result<(), String> {
    if bitcoin_cli(compose_dir, &["-rpcwallet=miner", "getwalletinfo"]).is_err() {
        let dir = bitcoin_cli(compose_dir, &["listwalletdir"])?;
        if dir.contains("\"miner\"") {
            bitcoin_cli(compose_dir, &["loadwallet", "miner"])?;
        } else {
            bitcoin_cli(compose_dir, &["createwallet", "miner"])?;
        }
    }

    // Make sure the chain has matured coinbase that the miner wallet can
    // spend before any `fund_role` calls `sendtoaddress`. Coinbase needs
    // 100 confirmations to mature, so a fresh regtest stack needs ≥101
    // blocks before the first reward is spendable. If `make regtest-up`
    // was run without a prior `regtest-mine`, the chain is at height 0
    // and the bootstrap's first `sendtoaddress` would fail with an
    // opaque error.
    let tip = bitcoin_cli(compose_dir, &["getblockcount"])?
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    if tip < 101 {
        let addr = bitcoin_cli(
            compose_dir,
            &["-rpcwallet=miner", "getnewaddress", "", "bech32"],
        )?;
        let n = (101u64 - tip).to_string();
        bitcoin_cli(
            compose_dir,
            &["-rpcwallet=miner", "generatetoaddress", &n, addr.trim()],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Discovery + parsing helpers
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn env_or_default(key: &str, default: PathBuf) -> PathBuf {
    std::env::var(key).map(PathBuf::from).unwrap_or(default)
}

fn rgb_path(tools_dir: &Path) -> PathBuf {
    tools_dir.join("rgb-cmd/bin/rgb")
}

fn require_tools(tools_dir: &Path) -> Result<(), String> {
    let rgb = rgb_path(tools_dir);
    if !rgb.is_file() {
        return Err(format!(
            "rgb-cmd missing at {rgb:?}.\n\
             Run: make -C infra/regtest rgb-tools-install",
        ));
    }
    Ok(())
}

fn require_stack_up(compose_dir: &Path, electrum_url: &str) -> Result<(), String> {
    if bitcoin_cli(compose_dir, &["getblockchaininfo"]).is_err() {
        return Err("bitcoind not reachable via `docker compose exec` from \
             {compose_dir:?}. Run: make -C infra/regtest regtest-up"
            .to_owned());
    }
    let host_port = electrum_url
        .trim_start_matches("tcp://")
        .trim_start_matches("ssl://");
    let addr: std::net::SocketAddr = host_port
        .to_socket_addrs()
        .map_err(|e| format!("bad ELECTRUM_URL `{electrum_url}`: {e}"))?
        .next()
        .ok_or_else(|| format!("ELECTRUM_URL `{electrum_url}` resolved to no addresses"))?;
    if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)).is_err() {
        return Err(format!(
            "electrs not reachable at {electrum_url}. \
             Run: make -C infra/regtest regtest-up"
        ));
    }
    Ok(())
}

use std::net::ToSocketAddrs;

/// Monotonic-per-process counter so multiple `taker_consignment_for` calls
/// within a single test don't collide on the tempdir filename.
fn uniq_label() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{}", N.fetch_add(1, Ordering::Relaxed))
}

fn last_word(s: &str) -> Option<String> {
    s.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| l.split_whitespace().last())
        .map(|w| w.to_owned())
}

/// Find the first `<base>#<label>` token (stripping any `prefix:` segments,
/// e.g. `rgb:sch:`). Mirrors the awk in `rgb-issue-asset:49`.
fn parse_schema_id(schemata_out: &str) -> Option<String> {
    for line in schemata_out.lines() {
        for tok in line.split_whitespace() {
            if !tok.contains('#') {
                continue;
            }
            let id = tok.rsplit(':').next().unwrap_or(tok);
            if id.contains('#') {
                return Some(id.to_owned());
            }
        }
    }
    None
}

/// All keychain-9 outpoints in `rgb utxos` output. Entries span two lines —
/// `<addr>    &9/<idx>` then `<height>    <amount>    <txid>:<vout>` — so we
/// scan for a `&9/` marker and pull the outpoint from the following non-empty
/// line.
fn parse_all_keychain9_outpoints(utxos_out: &str) -> Vec<Outpoint> {
    let mut out = Vec::new();
    let mut lines = utxos_out.lines();
    while let Some(line) = lines.next() {
        if !line.contains("&9/") {
            continue;
        }
        for next in lines.by_ref() {
            if next.trim().is_empty() {
                continue;
            }
            for tok in next.split_whitespace() {
                if is_outpoint(tok) {
                    if let Ok(op) = tok.parse::<Outpoint>() {
                        out.push(op);
                    }
                    break;
                }
            }
            break;
        }
    }
    out
}

/// First `txid:vout` on a line that mentions keychain 9, else the first
/// `txid:vout` anywhere in the output. Mirrors `rgb-issue-asset:62-70`.
fn parse_keychain9_outpoint(utxos_out: &str) -> Option<String> {
    for line in utxos_out.lines() {
        if line.contains("keychain=9")
            || line.contains("&9/")
            || line.trim_start().starts_with("9 ")
        {
            for tok in line.split_whitespace() {
                if is_outpoint(tok) {
                    return Some(tok.to_owned());
                }
            }
        }
    }
    for line in utxos_out.lines() {
        for tok in line.split_whitespace() {
            if is_outpoint(tok) {
                return Some(tok.to_owned());
            }
        }
    }
    None
}

fn is_outpoint(s: &str) -> bool {
    let parts: Vec<_> = s.splitn(2, ':').collect();
    parts.len() == 2
        && parts[0].len() == 64
        && parts[0].chars().all(|c| c.is_ascii_hexdigit())
        && parts[1].parse::<u32>().is_ok()
}

/// Substitute the two dynamic lines in the contract YAML template — same
/// `sed` substitutions `rgb-issue-asset:83-86` does.
fn render_yaml_template(template: &str, schema_id: &str, seal_outpoint: &str) -> String {
    let mut out: Vec<String> = template
        .lines()
        .map(|line| {
            if line.starts_with("schema:") {
                format!("schema: {schema_id}")
            } else if line.starts_with("    seal:") {
                format!("    seal: {seal_outpoint}")
            } else {
                line.to_owned()
            }
        })
        .collect();
    if template.ends_with('\n') {
        out.push(String::new());
    }
    out.join("\n")
}

fn parse_contract_id(issue_out: &str) -> Option<String> {
    issue_out
        .split_whitespace()
        .filter(|tok| tok.starts_with("rgb:"))
        .last()
        .map(|s| s.to_owned())
}

fn parse_invoice(invoice_out: &str) -> Option<String> {
    invoice_out
        .lines()
        .rev()
        .find(|l| l.trim().starts_with("rgb:"))
        .map(|l| l.trim().to_owned())
}
