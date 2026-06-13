use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use async_trait::async_trait;
use rfq_types::{AssetId, Outpoint, RgbInventoryUtxo, SwapTransfer};
use thiserror::Error;

mod lib_backend;
mod swap;
mod taker;
pub use lib_backend::{LibRgbBackend, WalletUtxo};
pub use swap::enrich_psbt_input;
pub use swap::SEAL_ANCHOR_SATS;
pub use taker::Taker;

/// Self-bootstrapping regtest harness shared across rfq-rgb's own e2e
/// tests and dependent crates' tests (e.g. maker-node's HTTP-layer
/// integration test). Gated behind the `test-helpers` feature
/// (default-on for workspace use; production consumers opt out via
/// `default-features = false`) so docker / subprocess machinery is
/// excluded from minimal builds.
#[cfg(feature = "test-helpers")]
pub mod test_helpers;

/// Re-export of the bitcoin prevout type — a sell-side swap PSBT pins each RGB
/// input's `scriptPubKey` + `value_sats`, which the consignment doesn't carry.
pub use rfq_btc::TxOut;

/// Re-exports of the RGB types that cross the sell-side trait boundary.
/// Callers (rfq-maker, mock backend) parse the maker's own invoice once and
/// thread these typed values into `create_swap_psbt_sell` instead of re-parsing
/// inside every backend impl. Lets downstream crates avoid an `rgb` dep.
pub use rgb::{ContractId, SecretSeal};

/// What the maker learns from a validated taker consignment on the sell side.
///
/// `outpoints` are the **input** outpoints of the consignment's terminal
/// transition — i.e. the bitcoin outpoints currently carrying the taker's
/// RGB, which `create_swap_psbt_sell` will pin as PSBT inputs. *Not* the
/// transition's output destinations (the maker's blinded seal + the taker's
/// witness-vout change seal) — those are post-transition state on a witness
/// tx that hasn't broadcast yet, so they're useless to the swap composition.
///
/// `total_amount` sums the spendable RGB at those input outpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsignmentInfo {
    pub total_amount: u64,
    pub outpoints: Vec<Outpoint>,
}

/// Output of `finalize_after_taker_sign`: the finalized witness transaction
/// ready to hand to `BitcoinClient::broadcast`, plus the witness txid and the
/// witness-extended consignment the RGB receiver imports post-broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedSwap {
    pub raw_tx: Vec<u8>,
    pub witness_txid: String,
    pub final_consignment_base64: String,
}

/// What the maker needs back from its own invoice string at sell-side /sign
/// time. The maker minted the invoice via `create_invoice`, shipped the wire
/// string to the taker on the `Quote`, and now needs the typed contract id (so
/// rfq-maker doesn't depend on `rgb` to parse the wire string itself) plus the
/// optional assignment amount. The invoice's beneficiary seal is NOT needed: the
/// maker rebuilds the transition and routes its own receive to a fresh
/// witness-vout output, so the handshake seal (blinded or witness-vout) is just
/// the taker's consignment target.
#[derive(Debug, Clone)]
pub struct MakerInvoiceParts {
    pub contract_id: ContractId,
    pub amount: Option<u64>,
}

#[derive(Debug, Error)]
pub enum RgbError {
    #[error("invalid RGB invoice")]
    InvalidInvoice,
    #[error("failed to load stash: {0}")]
    StashLoad(String),
    #[error("contract not found in stash: {0}")]
    ContractNotFound(String),
    #[error("failed to build transfer: {0}")]
    TransferBuild(String),
    #[error("failed to finalize signed PSBT: {0}")]
    FinalizeFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait RgbBackend: Send + Sync {
    /// Per-UTXO view of inventory. Each entry corresponds to one bitcoin
    /// outpoint holding an RGB allocation for `asset`.
    async fn list_inventory_utxos(
        &self,
        asset: &AssetId,
    ) -> Result<Vec<RgbInventoryUtxo>, RgbError>;

    async fn validate_invoice(&self, invoice: &str) -> Result<(), RgbError>;

    /// Display metadata for `asset` — `(ticker, decimal_precision)` read from the
    /// RGB contract's `spec`. Makers advertise this so the broker can serve an
    /// asset directory.
    async fn asset_spec(&self, asset: &AssetId) -> Result<(String, u8), RgbError>;

    /// Buy-side swap PSBT construction (declared-funding model). The maker
    /// contributes its RGB-bearing inputs (`maker_rgb_utxos`, the outpoints the
    /// inventory store reserved) and the RGB transition to `rgb_invoice`. The
    /// taker's BTC funding inputs are discovered up front from its declared
    /// `btc_funding_addr` (resolved to `taker_btc_inputs` by the caller via
    /// `BitcoinClient::list_unspent` + coin selection) and built into the PSBT
    /// here, so the taker only signs — it never restructures the tx. Outputs:
    /// the maker's BTC payout (`gross_btc_sats`), the taker's BTC change (back
    /// to `btc_funding_addr`), and the RGB commitment. `actual_fee_sats` is the
    /// taker-paid network fee deducted from the change.
    ///
    /// Because the full input set is final at build time, the witness txid is
    /// stable: `partial_psbt` is maker-RGB-side-signed, `consignment` is
    /// `Some`, and `expected_witness_txid` is `Some`.
    /// `change_rungs` (usually empty) piggybacks a denomination-ladder split of
    /// the maker's RGB change onto this settlement tx: each entry becomes a fresh
    /// keychain-0 output carrying that many RGB units, with the remainder on the
    /// host. Empty ⇒ the change rides the host as one allocation (unchanged).
    #[allow(clippy::too_many_arguments)]
    async fn create_swap_psbt_buy(
        &self,
        rgb_invoice: &str,
        amount: u64,
        maker_rgb_utxos: &[Outpoint],
        taker_btc_inputs: &[(Outpoint, TxOut)],
        btc_funding_addr: &str,
        gross_btc_sats: u64,
        actual_fee_sats: u64,
        change_rungs: &[u64],
    ) -> Result<SwapTransfer, RgbError>;

    /// Finalize a fully-signed swap PSBT: extract the witness tx ready to
    /// broadcast, and emit the witness-extended consignment. Broadcasting
    /// itself is the caller's job (via `BitcoinClient::broadcast`) — this
    /// trait stays bitcoin-network-free.
    async fn finalize_after_taker_sign(
        &self,
        signed_psbt_base64: &str,
        original_consignment_base64: &str,
    ) -> Result<FinalizedSwap, RgbError>;

    /// Maker-side, sell flow: mint an RGB invoice binding the contract and the
    /// expected `amount` to a maker-controlled seal. Published on the sell-side
    /// `Quote` as `maker_rgb_invoice`; the taker builds a consignment to it.
    async fn create_invoice(&self, asset: &AssetId, amount: u64) -> Result<String, RgbError>;

    /// Maker-side, sell flow: parse the maker's own invoice (the one returned
    /// by `create_invoice`) back into the typed components
    /// `create_swap_psbt_sell` needs. Called once at /sign time on the wire
    /// string that was stored on the `Quote`; lets rfq-maker thread typed
    /// `ContractId` + `SecretSeal` into the swap-PSBT call without depending
    /// on `rgb` directly. Mock backends may fake the components from the
    /// invoice string.
    async fn parse_maker_invoice(&self, invoice: &str) -> Result<MakerInvoiceParts, RgbError>;

    /// Maker-side, sell flow: validate a taker-submitted consignment against
    /// the maker's expected contract and return the RGB input outpoints +
    /// total amount (see [`ConsignmentInfo`]). `Err(TransferBuild)` when the
    /// consignment is malformed or doesn't match `expected_contract_id`.
    ///
    /// Takes a typed [`ContractId`] rather than the maker's invoice string
    /// — the maker minted the invoice itself in `create_invoice` and only
    /// needs the contract id here for cross-checking; rfq-maker already
    /// parses the invoice once at /sign time (via `parse_maker_invoice`)
    /// so reusing the typed `contract_id` from `MakerInvoiceParts` avoids
    /// a self-round-trip inside the backend.
    /// `consigned_outpoints` are the taker's RGB UTXOs being sold, named by the
    /// taker on the wire (a provenance consignment proves the asset + history but
    /// does NOT say which outpoints are being offered — that's the holder's input;
    /// see docs/provenance-consignment-proposal.md). The backend accepts the
    /// consignment, then confirms the claimed RGB actually sits at those outpoints.
    async fn validate_incoming_consignment(
        &self,
        consignment_base64: &str,
        expected_contract_id: ContractId,
        consigned_outpoints: &[Outpoint],
    ) -> Result<ConsignmentInfo, RgbError>;

    /// Sell-side swap PSBT construction. Inputs: the taker's RGB-bearing
    /// outpoints (with prevout data) plus the maker's BTC funding inputs.
    /// Outputs: the taker's BTC payout (`gross_btc_sats - actual_fee_sats`),
    /// maker BTC change, and taker RGB change to `rgb_change_invoice` if any.
    /// The maker signs its own inputs.
    ///
    /// The maker's own RGB destination is passed as typed values:
    /// `contract_id`, `maker_seal` (the blinded seal from the invoice the maker
    /// minted in `create_invoice`), and `deliver_amount` (the amount the maker
    /// expects to receive — surplus from `consignment_info.total_amount` goes
    /// to `rgb_change_invoice`). The caller (rfq-maker) parses the maker's
    /// invoice once at /sign time and threads these in; the backend doesn't
    /// re-parse the invoice it just minted.
    ///
    /// All inputs are committed here, so `expected_witness_txid` is `Some(_)` —
    /// unlike the buy side, where the taker's inputs are still outstanding.
    /// `btc_change_rungs` (usually empty) piggybacks a BTC-denomination-ladder
    /// split of the maker's BTC change onto this settlement tx: each entry becomes
    /// a fresh keychain-0 output of that many sats, the remainder staying on the
    /// change address. Sells deplete the k0 BTC pool, so this keeps it laddered.
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
    ) -> Result<SwapTransfer, RgbError>;

    /// Sell-side `/sign` guard: confirm a taker-signed swap PSBT still spends
    /// every RGB outpoint the validated consignment named (`consigned_outpoints`)
    /// and still commits to the witness txid fixed at build time
    /// (`expected_witness_txid`). Replaces the maker's old substring scan of the
    /// PSBT string, which only worked against the mock backend's plaintext
    /// PSBTs — a real base64 PSBT carries prevouts as raw (byte-reversed) bytes,
    /// never as the ASCII `txid:vout` form. `Err(TransferBuild)` carries the
    /// human-readable reason; the maker maps it to a 400.
    async fn verify_signed_swap_psbt(
        &self,
        signed_psbt_base64: &str,
        consigned_outpoints: &[Outpoint],
        expected_witness_txid: &str,
    ) -> Result<(), RgbError>;
}

#[derive(Debug, Clone)]
pub struct MockRgbBackend {
    utxos: Vec<RgbInventoryUtxo>,
    /// Monotonic counter so successive `create_invoice` calls mint distinct
    /// invoice strings. Shared across clones — a cloned backend keeps minting
    /// from the same sequence.
    invoice_nonce: Arc<AtomicU64>,
}

impl MockRgbBackend {
    pub fn new(utxos: Vec<RgbInventoryUtxo>) -> Self {
        Self {
            utxos,
            invoice_nonce: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Prefix of a mock sell-side consignment. The taker builds the rest; see
/// `MockRgbBackend::validate_incoming_consignment` for the field layout.
pub const MOCK_SELL_CONSIGNMENT_PREFIX: &str = "mock-consignment|sell|";

#[async_trait]
impl RgbBackend for MockRgbBackend {
    async fn list_inventory_utxos(
        &self,
        asset: &AssetId,
    ) -> Result<Vec<RgbInventoryUtxo>, RgbError> {
        Ok(self
            .utxos
            .iter()
            .filter(|utxo| utxo.asset_id == *asset)
            .cloned()
            .collect())
    }

    async fn validate_invoice(&self, invoice: &str) -> Result<(), RgbError> {
        if invoice.starts_with("rgb:") {
            Ok(())
        } else {
            Err(RgbError::InvalidInvoice)
        }
    }

    async fn asset_spec(&self, _asset: &AssetId) -> Result<(String, u8), RgbError> {
        Ok(("MOCK".to_owned(), 0))
    }

    async fn create_swap_psbt_buy(
        &self,
        rgb_invoice: &str,
        amount: u64,
        maker_rgb_utxos: &[Outpoint],
        taker_btc_inputs: &[(Outpoint, TxOut)],
        btc_funding_addr: &str,
        gross_btc_sats: u64,
        actual_fee_sats: u64,
        _change_rungs: &[u64],
    ) -> Result<SwapTransfer, RgbError> {
        self.validate_invoice(rgb_invoice).await?;

        // Deterministic mock PSBT. Declared-funding: the taker's BTC inputs are
        // present at build time, so the witness txid is stable (committed via
        // `:wt=`). `rgb_in`/`btc_in` list every input verbatim; `maker-signed`
        // stands in for the maker's signatures over its own RGB inputs. Real
        // bytes land with #13's LibRgbBackend.
        let rgb_in = join_outpoints(maker_rgb_utxos.iter());
        let btc_in = join_outpoints(taker_btc_inputs.iter().map(|(o, _)| o));
        let taker_btc_total: u64 = taker_btc_inputs.iter().map(|(_, t)| t.value_sats).sum();
        let taker_change = taker_btc_total
            .saturating_sub(gross_btc_sats)
            .saturating_sub(actual_fee_sats);
        let body = format!(
            "mock-psbt:buy:invoice={rgb_invoice}:amount={amount}:funding_addr={btc_funding_addr}\
             :gross={gross_btc_sats}:fee={actual_fee_sats}:taker_change={taker_change}\
             :rgb_in=[{rgb_in}]:btc_in=[{btc_in}]:maker-signed",
        );
        let partial_psbt = format!("{body}:wt={}", mock_witness_txid(&body));
        Ok(SwapTransfer {
            expected_witness_txid: Some(extract_witness_txid(&partial_psbt)),
            partial_psbt,
            consignment: Some(format!("mock-consignment:buy:amount={amount}")),
        })
    }

    async fn finalize_after_taker_sign(
        &self,
        signed_psbt_base64: &str,
        original_consignment_base64: &str,
    ) -> Result<FinalizedSwap, RgbError> {
        if signed_psbt_base64.is_empty() {
            return Err(RgbError::FinalizeFailed("empty signed PSBT".to_owned()));
        }
        Ok(FinalizedSwap {
            raw_tx: signed_psbt_base64.as_bytes().to_vec(),
            witness_txid: extract_witness_txid(signed_psbt_base64),
            final_consignment_base64: format!("final:{original_consignment_base64}"),
        })
    }

    async fn create_invoice(&self, asset: &AssetId, amount: u64) -> Result<String, RgbError> {
        let nonce = self.invoice_nonce.fetch_add(1, Ordering::Relaxed);
        Ok(format!("rgb:mock-invoice:{}:{amount}:{nonce}", asset.id))
    }

    async fn parse_maker_invoice(&self, invoice: &str) -> Result<MakerInvoiceParts, RgbError> {
        // Parse `rgb:mock-invoice:<asset_id>:<amount>:<nonce>`. The typed
        // ContractId/SecretSeal are derived deterministically from the
        // invoice string (just hashing) — real RGB cryptography isn't needed
        // for the mock's downstream consumers; only round-trip stability is.
        let rest = invoice
            .strip_prefix("rgb:mock-invoice:")
            .ok_or_else(|| RgbError::TransferBuild(format!("not a mock invoice: {invoice}")))?;
        let mut parts = rest.splitn(3, ':');
        let _asset_id = parts
            .next()
            .ok_or_else(|| RgbError::TransferBuild("mock invoice: missing asset_id".to_owned()))?;
        let amount: u64 = parts
            .next()
            .ok_or_else(|| RgbError::TransferBuild("mock invoice: missing amount".to_owned()))?
            .parse()
            .map_err(|_| RgbError::TransferBuild("mock invoice: bad amount".to_owned()))?;
        Ok(MakerInvoiceParts {
            contract_id: ContractId::from(mock_bytes32(&format!("{invoice}|contract"))),
            amount: Some(amount),
        })
    }

    async fn validate_incoming_consignment(
        &self,
        consignment_base64: &str,
        expected_contract_id: ContractId,
        consigned_outpoints: &[Outpoint],
    ) -> Result<ConsignmentInfo, RgbError> {
        // Real Stock validation lands in `LibRgbBackend`. The mock parses a
        // deterministic pipe-delimited blob the taker builds:
        //   mock-consignment|sell|invoice=<inv>|amount=<n>|outpoints=<csv>
        // and rejects the sentinel `rgb-invalid` so tests can drive the
        // consignment-rejected path.
        if consignment_base64.contains("rgb-invalid") {
            return Err(RgbError::TransferBuild(
                "consignment failed stock validation".to_owned(),
            ));
        }
        let body = consignment_base64
            .strip_prefix(MOCK_SELL_CONSIGNMENT_PREFIX)
            .ok_or_else(|| RgbError::TransferBuild("not a sell-side consignment".to_owned()))?;

        let mut amount = None;
        let mut outpoints: Vec<Outpoint> = Vec::new();
        for field in body.split('|') {
            // `invoice=` is ignored if present (provenance model — no maker invoice).
            if let Some(v) = field.strip_prefix("amount=") {
                amount = Some(
                    v.parse::<u64>()
                        .map_err(|_| RgbError::TransferBuild("bad amount field".to_owned()))?,
                );
            } else if let Some(v) = field.strip_prefix("outpoints=") {
                for op in v.split(',').filter(|s| !s.is_empty()) {
                    outpoints
                        .push(op.parse::<Outpoint>().map_err(|_| {
                            RgbError::TransferBuild(format!("bad outpoint `{op}`"))
                        })?);
                }
            }
        }

        // The real backend verifies the contract against the accepted consignment;
        // the mock accepts `expected_contract_id` as given (only the `rgb-invalid`
        // sentinel above drives the rejection path).
        let _ = expected_contract_id;
        let total_amount =
            amount.ok_or_else(|| RgbError::TransferBuild("missing amount field".to_owned()))?;
        // Provenance model: the taker names its outpoints on the wire. Prefer those;
        // fall back to the blob-embedded list so existing fixtures keep working.
        let outpoints = if consigned_outpoints.is_empty() {
            outpoints
        } else {
            consigned_outpoints.to_vec()
        };
        if outpoints.is_empty() {
            return Err(RgbError::TransferBuild(
                "consignment names no outpoints".to_owned(),
            ));
        }

        Ok(ConsignmentInfo {
            total_amount,
            outpoints,
        })
    }

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
        _btc_change_rungs: &[u64],
    ) -> Result<SwapTransfer, RgbError> {
        let rgb_in = join_outpoints(taker_rgb_prevouts.iter().map(|(o, _)| o));
        let btc_in = join_outpoints(maker_btc_inputs.iter().map(|(o, _)| o));
        let payout = gross_btc_sats.saturating_sub(actual_fee_sats);
        // Deterministic mock PSBT. `rgb_in=[...]` lists every consigned
        // outpoint verbatim so the maker's bait-and-switch check at /sign can
        // confirm the signed PSBT still spends them. `maker-signed` stands in
        // for the maker's signatures over its own BTC inputs.
        let body = format!(
            "mock-psbt:sell:contract={contract_id}\
             :deliver={deliver_amount}:payout_addr={btc_payout_addr}\
             :gross={gross_btc_sats}:fee={actual_fee_sats}:payout={payout}\
             :rgb_amount={}:rgb_in=[{rgb_in}]:btc_in=[{btc_in}]:rgb_change={}:maker-signed",
            consignment_info.total_amount,
            rgb_change_invoice.unwrap_or("none"),
        );
        // All inputs are committed, so the witness txid is stable from here.
        let partial_psbt = format!("{body}:wt={}", mock_witness_txid(&body));
        Ok(SwapTransfer {
            expected_witness_txid: Some(extract_witness_txid(&partial_psbt)),
            partial_psbt,
            // Sell side: the taker supplied the consignment; the maker has
            // none of its own to hand back.
            consignment: None,
        })
    }

    async fn verify_signed_swap_psbt(
        &self,
        signed_psbt_base64: &str,
        consigned_outpoints: &[Outpoint],
        expected_witness_txid: &str,
    ) -> Result<(), RgbError> {
        // Mock PSBTs are plaintext: the consigned outpoints appear as
        // `txid:vout` inside `rgb_in=[...]` and the witness id after `:wt=`.
        // This mirrors the substring guard that used to live in rfq-maker.
        for outpoint in consigned_outpoints {
            if !signed_psbt_base64.contains(&outpoint.to_string()) {
                return Err(RgbError::TransferBuild(
                    "input set diverged from consignment".to_owned(),
                ));
            }
        }
        if !signed_psbt_base64.contains(expected_witness_txid) {
            return Err(RgbError::TransferBuild("txid mismatch".to_owned()));
        }
        Ok(())
    }
}

fn join_outpoints<'a>(outpoints: impl Iterator<Item = &'a Outpoint>) -> String {
    outpoints
        .map(|o| o.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Deterministic 32-byte hash of a string, used by mock impls to fake out the
/// typed `ContractId` / `SecretSeal` values the real backend would derive from
/// an `RgbInvoice`. Not cryptographic — just stable across calls so tests can
/// re-derive the same bytes.
fn mock_bytes32(s: &str) -> [u8; 32] {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    let h = hasher.finish().to_le_bytes();
    let mut out = [0u8; 32];
    for chunk in out.chunks_mut(8) {
        chunk.copy_from_slice(&h);
    }
    out
}

/// Deterministic 64-hex mock txid derived from the signed PSBT string. Lets
/// tests assert a stable witness txid without a real bitcoin tx serializer.
fn mock_witness_txid(signed_psbt: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    signed_psbt.hash(&mut hasher);
    let h = hasher.finish();
    format!("{h:064x}")
}

/// Read back the witness txid a sell-side PSBT embeds (`:wt=<64-hex>`). With
/// every input committed at build time the txid is stable, so the maker can
/// publish it as `expected_witness_txid` and re-check it at `/sign`. Buy-side
/// PSBTs carry no `wt=` token — the taker's BTC inputs land only at `/sign` —
/// so there it falls back to hashing the signed bytes.
fn extract_witness_txid(psbt: &str) -> String {
    if let Some(idx) = psbt.find("wt=") {
        let wt: String = psbt[idx + 3..].chars().take(64).collect();
        if wt.len() == 64 && wt.chars().all(|c| c.is_ascii_hexdigit()) {
            return wt;
        }
    }
    mock_witness_txid(psbt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_types::{AssetKind, BitcoinNetwork, Outpoint};

    fn asset(id: &str) -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: id.to_owned(),
        }
    }

    fn utxo(asset: AssetId, idx: usize, amount: u64) -> RgbInventoryUtxo {
        RgbInventoryUtxo {
            outpoint: Outpoint {
                txid: format!("{idx:064x}"),
                vout: 0,
            },
            asset_id: asset,
            amount,
            btc_sats: 0,
        }
    }

    #[tokio::test]
    async fn mock_list_inventory_utxos_filters_by_asset() {
        let target = asset("rgb-target");
        let other = asset("rgb-other");
        let backend = MockRgbBackend::new(vec![
            utxo(target.clone(), 0, 100),
            utxo(other.clone(), 1, 50),
            utxo(target.clone(), 2, 200),
        ]);

        let utxos = backend.list_inventory_utxos(&target).await.unwrap();

        assert_eq!(utxos.len(), 2);
        assert_eq!(utxos[0].amount, 100);
        assert_eq!(utxos[1].amount, 200);
        for u in &utxos {
            assert_eq!(u.asset_id, target);
        }
    }

    #[tokio::test]
    async fn mock_list_inventory_utxos_is_deterministic() {
        let target = asset("rgb-target");
        let backend = MockRgbBackend::new(vec![
            utxo(target.clone(), 0, 1),
            utxo(target.clone(), 1, 2),
            utxo(target.clone(), 2, 3),
        ]);

        let first = backend.list_inventory_utxos(&target).await.unwrap();
        let second = backend.list_inventory_utxos(&target).await.unwrap();
        assert_eq!(first, second, "mock should be deterministic across calls");
    }
}
