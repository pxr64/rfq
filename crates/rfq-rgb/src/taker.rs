//! Production taker counterparty for BTC↔RGB atomic swaps.
//!
//! The maker side is [`LibRgbBackend`] (the `RgbBackend` trait); the taker is
//! the counterparty that owns BTC inputs (buy) or RGB inputs (sell), mints its
//! own RGB invoice, builds its sell consignment, and applies signatures to the
//! maker-built PSBT. This logic used to live only in the test harness
//! (`test_helpers::TakerGuard`); it's promoted here so non-test drivers (a
//! future client wallet) can reuse it. `TakerGuard` now delegates to `Taker`.
//!
//! `Taker`'s read paths reuse `LibRgbBackend` (role-agnostic). The bp-wallet
//! ops (`lookup_prevout`, `spare_btc_input`, `sign_and_finalize`) load the
//! taker's own wallet cache directly, since signing a maker-built PSBT is the
//! counterparty's job, not part of the maker-side `RgbBackend` abstraction.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;

use bpstd::psbt::{Psbt, PsbtConstructor};
use bpstd::signers::TestnetRefSigner;
use bpstd::{Address, Derive, Sats, Terminal, XprivAccount, XpubDerivable};
use bpwallet::fs::FsTextStore;
use bpwallet::hot::SecureIo;
use bpwallet::Wallet as BpWallet;
use rfq_types::{AssetId, Outpoint, RgbInventoryUtxo};
use rgb::invoice::{Beneficiary, RgbInvoice};
use rgb::{ContractId, RgbDescr};

use crate::{enrich_psbt_input, LibRgbBackend, RgbBackend, RgbError, TxOut};

/// Pre-sign safety gate for [`Taker::sign_and_finalize`] (GitHub issue #38). A consignment is
/// unsigned history; the taker's Bitcoin signature is what authorizes the swap — so before signing
/// the maker-built PSBT the taker asserts *which inputs it signs* and *what it receives*. Each
/// `None` field skips its check; a production caller populates the ones relevant to its leg
/// (passing `None` overall = no gate — legacy/test paths only).
#[derive(Debug, Default, Clone)]
pub struct SignGuard {
    /// Whitelist of taker-owned inputs allowed to be signed. If `Some`, any taker-owned PSBT input
    /// outside it is rejected — the **sell-side anti-sweep** (the maker can't splice the taker's
    /// *other* RGB UTXOs into the tx). Sell passes the named sale outpoints.
    pub allowed_outpoints: Option<Vec<Outpoint>>,
    /// Blacklist of taker-owned inputs that must NOT be signed. The **buy side** passes the taker's
    /// RGB anchors: a buy pays BTC, so signing any RGB-bearing input the maker spliced in leaks it.
    pub forbidden_outpoints: Option<Vec<Outpoint>>,
    /// The swap tx the maker published; after decode `psbt.txid()` must equal it (anti-substitution).
    pub expected_witness_txid: Option<String>,
    /// `(taker BTC payout scriptPubkey bytes, minimum sats)` — outputs paying that script must total
    /// at least the minimum, so the maker can't redirect or short-change the taker's BTC payout.
    /// Build the script from the payout address with [`Taker::payout_spk`]. Sell side.
    pub expected_payout: Option<(Vec<u8>, u64)>,
}

/// A swap counterparty's RGB/BTC wallet. Constructed from the same on-disk
/// stash + signer inputs as [`LibRgbBackend`]; the wallet cache lives at
/// `<data_dir>/<network>/<wallet_name>` (rgb-cmd layout).
pub struct Taker {
    data_dir: PathBuf,
    wallet_name: String,
    network: String,
    electrum_url: String,
    account_file: PathBuf,
    signer_password: String,
}

impl Taker {
    pub fn new(
        data_dir: PathBuf,
        wallet_name: String,
        network: String,
        electrum_url: String,
        account_file: PathBuf,
        signer_password: String,
    ) -> Self {
        Self {
            data_dir,
            wallet_name,
            network,
            electrum_url,
            account_file,
            signer_password,
        }
    }

    /// Role-agnostic read/transfer backend over the taker's own stash.
    fn lib_backend(&self) -> LibRgbBackend {
        LibRgbBackend::new(
            self.data_dir.clone(),
            self.wallet_name.clone(),
            self.network.clone(),
            self.electrum_url.clone(),
            self.account_file.clone(),
            self.signer_password.clone(),
        )
    }

    fn wallet_path(&self) -> PathBuf {
        self.data_dir.join(&self.network).join(&self.wallet_name)
    }

    fn load_wallet(&self) -> Result<BpWallet<XpubDerivable, RgbDescr>, RgbError> {
        let provider = FsTextStore::new(self.wallet_path())
            .map_err(|e| RgbError::StashLoad(format!("taker wallet provider: {e}")))?;
        BpWallet::load(provider, true)
            .map_err(|e| RgbError::StashLoad(format!("taker wallet load: {e}")))
    }

    /// The taker's RGB inventory for `asset`.
    pub async fn inventory(&self, asset: &AssetId) -> Result<Vec<RgbInventoryUtxo>, RgbError> {
        self.lib_backend().list_inventory_utxos(asset).await
    }

    /// The contract's ticker + precision (decimals), for formatting inventory.
    pub fn contract_spec(&self, asset: &AssetId) -> Result<(String, u8), RgbError> {
        self.lib_backend().contract_spec(asset)
    }

    /// Mint a fresh RGB invoice on the taker's stash (buy: receive bought
    /// tokens; sell: receive over-consigned change).
    pub async fn create_invoice(&self, asset: &AssetId, amount: u64) -> Result<String, RgbError> {
        self.lib_backend().create_invoice(asset, amount).await
    }

    /// Refresh the taker's bp-wallet UTXO cache against electrum so
    /// post-broadcast change becomes visible to `spare_btc_input` /
    /// `lookup_prevout` / `sign_and_finalize`.
    pub async fn sync_wallet(&self) -> Result<(), RgbError> {
        self.lib_backend().sync_wallet().await
    }

    /// Absorb a maker-returned consignment into the taker's stash so the RGB it
    /// just received becomes visible: tokens bought (buy) or change from a sell.
    /// The maker hands this back as `SettlementIntent.final_consignment` after
    /// broadcasting the swap tx. The allocation surfaces in [`Self::inventory`]
    /// once the witness confirms and [`Self::sync_wallet`] refreshes the cache.
    pub async fn accept_consignment(
        &self,
        asset: &AssetId,
        consignment_base64: &str,
    ) -> Result<(), RgbError> {
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;
        self.lib_backend()
            .accept_incoming_transfer(consignment_base64, contract_id)
            .await
    }

    /// Buy-side gate: validate the maker-delivered consignment's ancestry is mined on-chain
    /// BEFORE signing/paying. `expected_witness_txid` is the swap tx (the one hop allowed to
    /// be unmined). See [`LibRgbBackend::validate_buy_consignment`].
    pub async fn validate_buy_consignment(
        &self,
        asset: &AssetId,
        consignment_base64: &str,
        expected_witness_txid: &str,
    ) -> Result<(), RgbError> {
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;
        self.lib_backend()
            .validate_buy_consignment(consignment_base64, contract_id, expected_witness_txid)
            .await
    }

    /// Total RGB a maker-delivered consignment lands on the taker's OWN wallet UTXOs (#38
    /// delivered-value): the change that came back on a sell, or the bought amount on a buy. A
    /// misrouted delivery returns 0. See [`LibRgbBackend::consignment_delivery_to_wallet`].
    pub async fn consignment_delivery_to_wallet(
        &self,
        asset: &AssetId,
        consignment_base64: &str,
    ) -> Result<u64, RgbError> {
        let contract_id = ContractId::from_str(&asset.id)
            .map_err(|e| RgbError::ContractNotFound(format!("invalid contract id: {e}")))?;
        self.lib_backend()
            .consignment_delivery_to_wallet(consignment_base64, contract_id)
            .await
    }

    /// #38 delivered-value gate: verify a maker-delivered consignment credits the expected RGB to
    /// the taker's OWN seal — the one it minted in `invoice`. For a **blinded** beneficiary (the
    /// default) the delivery lands on one of the taker's existing anchors, so it's read directly
    /// via [`Self::consignment_delivery_to_wallet`]: `exact` requires the credited amount to equal
    /// `expected` (a sell's change = `gross − sold`), else it must be `>= expected` (a buy = at
    /// least the requested amount). A **witness-vout** beneficiary rides a not-yet-broadcast swap
    /// output (no UTXO to read pre-broadcast), so it's skipped here and verified at accept time —
    /// a documented residual for the rare witness-vout fallback (`create_invoice` prefers blinded).
    pub async fn verify_delivery(
        &self,
        asset: &AssetId,
        consignment_base64: &str,
        invoice: &str,
        expected: u64,
        exact: bool,
    ) -> Result<(), RgbError> {
        let parsed = RgbInvoice::from_str(invoice).map_err(|_| RgbError::InvalidInvoice)?;
        match parsed.beneficiary.into_inner() {
            Beneficiary::BlindedSeal(_) => {
                let delivered = self
                    .consignment_delivery_to_wallet(asset, consignment_base64)
                    .await?;
                let ok = if exact {
                    delivered == expected
                } else {
                    delivered >= expected
                };
                if !ok {
                    return Err(RgbError::TransferBuild(format!(
                        "delivered-value check failed: consignment credits {delivered} RGB to our \
                         seals, expected {}{expected} — refusing to sign",
                        if exact { "exactly " } else { "at least " }
                    )));
                }
            }
            Beneficiary::WitnessVout(..) => {
                // The delivery rides a not-yet-broadcast swap output — no UTXO to read yet. The
                // standard post-broadcast accept validates it; nothing to assert here.
            }
        }
        Ok(())
    }

    /// Build the taker's sell consignment: a unilateral RGB transfer to the
    /// maker's invoice. The taker does NOT broadcast this — the maker anchors
    /// it into the swap tx and broadcasts. See
    /// [`LibRgbBackend::create_transfer_to_invoice`].
    pub async fn create_transfer_to_invoice(
        &self,
        recipient_invoice: &str,
        fee_sats: u64,
    ) -> Result<String, RgbError> {
        self.lib_backend()
            .create_transfer_to_invoice(recipient_invoice, fee_sats)
            .await
    }

    /// Export a **provenance** consignment for the taker's own `outpoints` — the
    /// sell-leg primitive: the taker proves its RGB + history, the maker spends
    /// those outpoints into the swap tx. No PSBT, no fee, no anchor. See
    /// [`LibRgbBackend::export_provenance`] and
    /// `docs/provenance-consignment-proposal.md`.
    pub fn export_provenance(
        &self,
        contract: &str,
        outpoints: &[String],
    ) -> Result<String, RgbError> {
        self.lib_backend().export_provenance(contract, outpoints)
    }

    /// Look up a specific taker UTXO and return its `(Outpoint, TxOut)`. Used
    /// to resolve the RGB-input prevouts a validated sell consignment reports.
    pub fn lookup_prevout(&self, want: &Outpoint) -> Result<(Outpoint, TxOut), RgbError> {
        let wallet = self.load_wallet()?;
        let utxo = wallet
            .utxos()
            .find(|u| {
                u.outpoint.txid.to_string() == want.txid && u.outpoint.vout.into_u32() == want.vout
            })
            .ok_or_else(|| {
                RgbError::TransferBuild(format!("taker outpoint {want} not in wallet"))
            })?;
        let derived = wallet
            .descriptor()
            .derive(utxo.terminal.keychain, utxo.terminal.index)
            .next()
            .ok_or_else(|| RgbError::TransferBuild("descriptor produced no script".to_owned()))?;
        let spk: Vec<u8> = derived.to_script_pubkey().as_slice().to_vec();
        Ok((
            want.clone(),
            TxOut {
                value_sats: utxo.value.sats(),
                script_pubkey: spk,
            },
        ))
    }

    /// Any taker-controlled UTXO that does **not** carry an RGB allocation for
    /// `asset`, paired with its prevout `TxOut`. Used as `taker_btc_inputs[0]`
    /// for the buy-side swap PSBT — must skip RGB-bearing outpoints so the swap
    /// tx doesn't silently consume the taker's RGB at the bitcoin layer.
    pub async fn spare_btc_input(&self, asset: &AssetId) -> Result<(Outpoint, TxOut), RgbError> {
        let rgb_utxos = self.inventory(asset).await?;
        let rgb_set: HashSet<(String, u32)> = rgb_utxos
            .iter()
            .map(|u| (u.outpoint.txid.clone(), u.outpoint.vout))
            .collect();

        let wallet = self.load_wallet()?;
        let descriptor = wallet.descriptor().clone();
        let utxo = wallet
            .utxos()
            .find(|u| {
                let key = (u.outpoint.txid.to_string(), u.outpoint.vout.into_u32());
                !rgb_set.contains(&key)
            })
            .ok_or_else(|| {
                RgbError::TransferBuild(
                    "no spare taker BTC outpoint — every wallet UTXO is RGB-bearing".to_owned(),
                )
            })?;
        let derived = descriptor
            .derive(utxo.terminal.keychain, utxo.terminal.index)
            .next()
            .ok_or_else(|| RgbError::TransferBuild("descriptor produced no script".to_owned()))?;
        let spk: Vec<u8> = derived.to_script_pubkey().as_slice().to_vec();
        Ok((
            Outpoint {
                txid: utxo.outpoint.txid.to_string(),
                vout: utxo.outpoint.vout.into_u32(),
            },
            TxOut {
                value_sats: utxo.value.sats(),
                script_pubkey: spk,
            },
        ))
    }

    /// Convert a BTC payout address into its scriptPubkey bytes — build
    /// [`SignGuard::expected_payout`] from the taker's own payout address with this.
    pub fn payout_spk(&self, address: &str) -> Result<Vec<u8>, RgbError> {
        Ok(Address::from_str(address)
            .map_err(|e| RgbError::TransferBuild(format!("bad payout address {address}: {e}")))?
            .script_pubkey()
            .as_slice()
            .to_vec())
    }

    /// Enrich every authorized PSBT input owned by the taker wallet, then sign + finalize against
    /// the taker descriptor. Inputs the taker doesn't own (the maker's, already carrying
    /// `partial_sigs`) are left untouched for the maker's own finalize step. Enrichment is mandatory:
    /// the maker leaves taker inputs with only `witness_utxo` + `sighash_type`, but `TestnetRefSigner`
    /// keys off `bip32_derivation`.
    ///
    /// **Pre-sign gate (GitHub #38):** when `guard` is `Some`, the taker verifies — *before producing
    /// any signature* — that the maker-built PSBT (a) signs only inputs the taker authorized (no
    /// spliced-in RGB UTXOs — the sweep), (d) pays the taker's BTC payout, and (e) is the swap tx the
    /// maker published. `None` skips the gate (legacy/test callers).
    pub fn sign_and_finalize(
        &self,
        partial_psbt_b64: &str,
        guard: Option<&SignGuard>,
    ) -> Result<String, RgbError> {
        let wallet = self.load_wallet()?;
        let descriptor = wallet.descriptor().clone();
        let owned: HashMap<(String, u32), (Terminal, Sats)> = wallet
            .utxos()
            .map(|u| {
                (
                    (u.outpoint.txid.to_string(), u.outpoint.vout.into_u32()),
                    (u.terminal, u.value),
                )
            })
            .collect();

        let account = XprivAccount::read(&self.account_file, &self.signer_password)
            .map_err(|e| RgbError::StashLoad(format!("taker account read: {e}")))?;
        let mut psbt = Psbt::from_base64(partial_psbt_b64)
            .map_err(|e| RgbError::TransferBuild(format!("decode partial PSBT: {e}")))?;

        // #38 pre-sign gate — assert what we sign + what we receive BEFORE any signature exists.
        // The checks are factored into `enforce_sign_guard` (pure, unit-tested) over a projection of
        // the PSBT, so the trust-critical logic is verifiable without a wallet.
        if let Some(g) = guard {
            let spent: Vec<(String, u32)> = psbt
                .inputs()
                .map(|inp| {
                    (
                        inp.previous_outpoint.txid.to_string(),
                        inp.previous_outpoint.vout.into_u32(),
                    )
                })
                .collect();
            let outputs: Vec<(Vec<u8>, u64)> = psbt
                .outputs()
                .map(|o| (o.script.as_slice().to_vec(), o.value().sats()))
                .collect();
            let owned_keys: HashSet<(String, u32)> = owned.keys().cloned().collect();
            enforce_sign_guard(&psbt.txid().to_string(), &spent, &outputs, &owned_keys, g)?;
        }

        let prev_outs: Vec<(usize, String, u32)> = psbt
            .inputs()
            .enumerate()
            .map(|(i, inp)| {
                (
                    i,
                    inp.previous_outpoint.txid.to_string(),
                    inp.previous_outpoint.vout.into_u32(),
                )
            })
            .collect();
        let mut enriched = 0usize;
        for (i, txid, vout) in prev_outs {
            // The guard (above) already rejected any taker-owned input it didn't authorize, so every
            // owned input here is safe to enrich + sign.
            if let Some((terminal, value)) = owned.get(&(txid, vout)).cloned() {
                // Taker inputs are its own untweaked receives/funding (only the
                // maker hosts tapret commitments), so the base derived script is
                // correct — no scriptPubkey match needed.
                enrich_psbt_input(&mut psbt, i, &descriptor, terminal, value, None)?;
                enriched += 1;
            }
        }
        if enriched == 0 {
            return Err(RgbError::TransferBuild(
                "no PSBT input matched a taker UTXO; check funding".to_owned(),
            ));
        }

        let signer = TestnetRefSigner::new(&account);
        let sig_count = psbt
            .sign(&signer)
            .map_err(|e| RgbError::TransferBuild(format!("taker sign: {e}")))?;
        if sig_count == 0 {
            return Err(RgbError::TransferBuild(
                "taker signer matched no inputs after enrichment".to_owned(),
            ));
        }
        psbt.finalize(&descriptor);
        Ok(psbt.to_base64())
    }
}

/// The pure #38 pre-sign checks over a *projected* PSBT — its txid, spent outpoints, and outputs as
/// `(scriptPubkey bytes, sats)` — plus the taker's owned-outpoint set. Extracted from
/// [`Taker::sign_and_finalize`] so the trust-critical logic is unit-testable without a wallet or
/// signing. Rejects: (e) a substituted txid, (d) a BTC payout that misses the taker's script or
/// falls short, or (a) any taker-owned input the guard did not authorize (the sweep).
fn enforce_sign_guard(
    psbt_txid: &str,
    spent_outpoints: &[(String, u32)],
    outputs: &[(Vec<u8>, u64)],
    owned: &HashSet<(String, u32)>,
    guard: &SignGuard,
) -> Result<(), RgbError> {
    // (e) anti-substitution: the tx we sign must be the swap the maker published (the unsigned-tx
    // txid is fixed pre-sign, so a divergence means the maker swapped the tx out from under us).
    if let Some(expected) = &guard.expected_witness_txid {
        if psbt_txid != expected {
            return Err(RgbError::TransferBuild(format!(
                "swap txid {psbt_txid} != expected {expected} — maker substituted the tx"
            )));
        }
    }
    // (d) the BTC payout must reach the taker's OWN script at >= the agreed minimum.
    if let Some((payout_spk, min_sats)) = &guard.expected_payout {
        let paid: u64 = outputs
            .iter()
            .filter(|(spk, _)| spk == payout_spk)
            .map(|(_, sats)| *sats)
            .sum();
        if paid < *min_sats {
            return Err(RgbError::TransferBuild(format!(
                "BTC payout to taker is {paid} sats < expected {min_sats} — maker redirected or \
                 short-changed the payout"
            )));
        }
    }
    // (a) input-set / anti-sweep: every taker-owned input must be authorized (whitelist for the sell
    // sale set, blacklist for the buy's RGB anchors). Maker-owned inputs aren't ours to police.
    let allowed: Option<HashSet<(String, u32)>> = guard
        .allowed_outpoints
        .as_ref()
        .map(|v| v.iter().map(|o| (o.txid.clone(), o.vout)).collect());
    let forbidden: Option<HashSet<(String, u32)>> = guard
        .forbidden_outpoints
        .as_ref()
        .map(|v| v.iter().map(|o| (o.txid.clone(), o.vout)).collect());
    for key in spent_outpoints {
        if !owned.contains(key) {
            continue;
        }
        if allowed.as_ref().is_some_and(|a| !a.contains(key)) {
            return Err(RgbError::TransferBuild(format!(
                "PSBT spends taker outpoint {}:{} outside the named sale set — refusing to sign \
                 (anti-sweep)",
                key.0, key.1
            )));
        }
        if forbidden.as_ref().is_some_and(|f| f.contains(key)) {
            return Err(RgbError::TransferBuild(format!(
                "PSBT spends taker RGB anchor {}:{} on a BTC-only leg — refusing to sign (anti-sweep)",
                key.0, key.1
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(txid: &str, vout: u32) -> Outpoint {
        Outpoint { txid: txid.to_owned(), vout }
    }
    fn owned(set: &[(&str, u32)]) -> HashSet<(String, u32)> {
        set.iter().map(|(t, v)| ((*t).to_owned(), *v)).collect()
    }
    fn spent(set: &[(&str, u32)]) -> Vec<(String, u32)> {
        set.iter().map(|(t, v)| ((*t).to_owned(), *v)).collect()
    }

    #[test]
    fn sell_signs_exactly_the_named_sale_set() {
        // Taker owns A + B (savings); the PSBT spends A (named) + a maker BTC input. Allowed.
        let g = SignGuard { allowed_outpoints: Some(vec![op("a", 0)]), ..Default::default() };
        enforce_sign_guard("tx", &spent(&[("a", 0), ("m", 0)]), &[], &owned(&[("a", 0), ("b", 1)]), &g)
            .expect("named sale set + maker input must sign");
    }

    #[test]
    fn sell_rejects_spliced_savings_utxo() {
        // The maker splices the taker's savings UTXO B into the tx → sweep → reject.
        let g = SignGuard { allowed_outpoints: Some(vec![op("a", 0)]), ..Default::default() };
        let err = enforce_sign_guard("tx", &spent(&[("a", 0), ("b", 1)]), &[], &owned(&[("a", 0), ("b", 1)]), &g)
            .unwrap_err();
        assert!(err.to_string().contains("anti-sweep"), "{err}");
    }

    #[test]
    fn buy_rejects_spliced_rgb_anchor() {
        // A buy pays BTC; the maker splices the taker's RGB anchor as an "input" → reject.
        let g = SignGuard { forbidden_outpoints: Some(vec![op("rgb", 0)]), ..Default::default() };
        let err = enforce_sign_guard("tx", &spent(&[("btc", 0), ("rgb", 0)]), &[], &owned(&[("btc", 0), ("rgb", 0)]), &g)
            .unwrap_err();
        assert!(err.to_string().contains("anti-sweep"), "{err}");
    }

    #[test]
    fn rejects_substituted_txid() {
        let g = SignGuard { expected_witness_txid: Some("expected".to_owned()), ..Default::default() };
        let err = enforce_sign_guard("different", &[], &[], &HashSet::new(), &g).unwrap_err();
        assert!(err.to_string().contains("substituted"), "{err}");
    }

    #[test]
    fn payout_must_reach_taker_at_minimum() {
        let spk = vec![0x00u8, 0x14, 0xde, 0xad, 0xbe, 0xef];
        let g = SignGuard { expected_payout: Some((spk.clone(), 1000)), ..Default::default() };
        // redirected: no output pays our script → 0 < 1000 → reject.
        assert!(enforce_sign_guard("tx", &[], &[(vec![0x99], 5000)], &HashSet::new(), &g).is_err());
        // shortfall: pays our script but below the minimum → reject.
        assert!(enforce_sign_guard("tx", &[], &[(spk.clone(), 999)], &HashSet::new(), &g).is_err());
        // exact (or over): accepted.
        enforce_sign_guard("tx", &[], &[(spk, 1000)], &HashSet::new(), &g).expect("sufficient payout signs");
    }

    #[test]
    fn empty_guard_is_permissive() {
        enforce_sign_guard("tx", &spent(&[("a", 0)]), &[], &owned(&[("a", 0)]), &SignGuard::default())
            .expect("a guard with no checks set permits");
    }
}
