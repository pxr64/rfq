use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Outpoint {
    pub txid: String,
    pub vout: u32,
}

impl Outpoint {
    pub fn new(txid: impl Into<String>, vout: u32) -> Self {
        Self {
            txid: txid.into(),
            vout,
        }
    }
}

impl std::fmt::Display for Outpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.txid, self.vout)
    }
}

impl std::str::FromStr for Outpoint {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (txid, vout) = s
            .rsplit_once(':')
            .ok_or_else(|| format!("outpoint missing ':' separator: {s:?}"))?;

        if txid.len() != 64 {
            return Err(format!(
                "outpoint txid must be 64 hex chars, got {}: {txid:?}",
                txid.len()
            ));
        }
        if !txid
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            return Err(format!(
                "outpoint txid must be lowercase hex (0-9a-f): {txid:?}"
            ));
        }
        let vout: u32 = vout
            .parse()
            .map_err(|e| format!("outpoint vout parse error: {e}"))?;

        Ok(Self {
            txid: txid.to_owned(),
            vout,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct RfqId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct QuoteId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct MakerId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct AssetId {
    pub network: BitcoinNetwork,
    pub kind: AssetKind,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum BitcoinNetwork {
    Mainnet,
    Testnet,
    Signet,
    Regtest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AssetKind {
    Btc,
    Rgb20,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteRequest {
    pub rfq_id: RfqId,
    pub base_asset: AssetId,
    pub quote_asset: AssetId,
    pub side: Side,
    pub amount: u64,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRfqRequest {
    pub base_asset: AssetId,
    pub quote_asset: AssetId,
    pub side: Side,
    pub amount: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub quote_id: QuoteId,
    pub rfq_id: RfqId,
    pub maker_id: MakerId,
    pub base_asset: AssetId,
    pub quote_asset: AssetId,
    pub side: Side,
    pub amount: u64,
    pub price: u64,
    pub expires_at_ms: u64,
    /// Maker's quote-time estimate of the network fee in sats. The taker pays
    /// the fee (see `docs/swap-flows.md`); this lets the taker see expected
    /// total (buy) or net (sell) up front. Zero until 15c wires real estimation.
    pub estimated_fee_sats: u64,
    /// Basis points the actual fee may exceed `estimated_fee_sats` at PSBT-build
    /// time before settlement aborts with `FeeSlippageExceeded`. Default 2000
    /// (= 20%).
    pub fee_slippage_bps: u16,
    /// Set on sell-side quotes only: the maker's RGB invoice the taker builds
    /// a consignment to. `None` on buy-side quotes. Populated in 16b/16c.
    pub maker_rgb_invoice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptQuoteRequest {
    pub quote_id: QuoteId,
    pub leg: SwapLeg,
}

/// Side-specific payload on `AcceptQuoteRequest`. See `docs/swap-flows.md` for
/// the full flow on each side.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "side", rename_all = "snake_case")]
pub enum SwapLeg {
    /// Taker is buying RGB and paying BTC. The taker declares the BTC address
    /// holding its funding UTXOs (`btc_funding_addr`); the maker discovers those
    /// UTXOs via `BitcoinClient::list_unspent`, builds them into the PSBT as
    /// inputs, and routes the taker's BTC change back to the same address. The
    /// taker only signs the inputs the maker built — it never restructures the
    /// PSBT. See `docs/swap-flows.md` (declared-funding buy side).
    Buy {
        rgb_invoice: String,
        btc_funding_addr: String,
    },
    /// Taker is selling RGB and receiving BTC. Maker publishes its own RGB
    /// invoice on the `Quote` (`maker_rgb_invoice`); taker delivers a
    /// consignment to that invoice via `/consignment`.
    Sell {
        btc_payout_addr: String,
        /// Where leftover RGB change goes back to the taker, if the consigned
        /// amount exceeds `quote.amount`. Optional — the maker MAY refuse
        /// non-exact consignments.
        rgb_change_invoice: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementIntent {
    pub quote_id: QuoteId,
    pub maker_id: MakerId,
    pub status: SettlementStatus,
    pub transfer: Option<SwapTransfer>,
    /// Deadline for the current settlement stage. Cleanup loop polls this.
    pub expires_at_ms: u64,
    /// Set once the maker has broadcast the witness tx (after `/sign`).
    pub witness_txid: Option<String>,
    /// Witness-extended consignment emitted post-broadcast. The RGB receiver
    /// imports this to advance their Stock.
    pub final_consignment: Option<String>,
}

/// Lifecycle state of a single settlement, from quote acceptance to a final
/// outcome. The valid transitions form this graph:
///
/// ```text
///   Pending ──▶ Accepted ──┐
///      │           │       │
///      └───────────┴──▶ AwaitingConsignment ──▶ AwaitingTakerSignature
///      │                                              │
///      └──────────────────────────────────────────────┤
///                                                      ▼
///                                          PendingBitcoinConfirm ──▶ Settled
///
///   any non-terminal state ──▶ Failed
/// ```
///
/// `Settled` and `Failed` are terminal. `can_transition_to` / `transition`
/// enforce the graph; `Pending` and `Accepted` are part of the lifecycle spec
/// but the mock maker currently emits intents starting at the `Awaiting*`
/// states (it has no pre-accept settlement object).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum SettlementStatus {
    /// Genesis: a settlement exists but the maker has taken no action yet.
    Pending,
    /// Quote accepted, side-specific work not yet started.
    Accepted,
    /// Sell-side only: maker is waiting for the taker to submit a consignment
    /// via `/consignment`.
    AwaitingConsignment,
    /// Maker has built the PSBT (with its inputs signed) and is waiting for
    /// the taker to submit a signed PSBT via `/sign`.
    AwaitingTakerSignature,
    /// Maker has broadcast the witness tx; awaiting bitcoin confirmation.
    PendingBitcoinConfirm,
    /// Tx confirmed; both legs final. Terminal.
    Settled,
    /// Settlement aborted (rejected consignment, fee slippage, broadcast
    /// failure, TTL lapse, reorg). Terminal.
    Failed,
}

impl SettlementStatus {
    /// True for states with no outgoing transitions — the settlement is over.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Settled | Self::Failed)
    }

    /// States reachable from `self` in one valid lifecycle step.
    pub fn allowed_next(self) -> &'static [SettlementStatus] {
        use SettlementStatus::*;
        match self {
            Pending => &[Accepted, AwaitingConsignment, AwaitingTakerSignature, Failed],
            Accepted => &[AwaitingConsignment, AwaitingTakerSignature, Failed],
            AwaitingConsignment => &[AwaitingTakerSignature, Failed],
            AwaitingTakerSignature => &[PendingBitcoinConfirm, Failed],
            PendingBitcoinConfirm => &[Settled, Failed],
            Settled | Failed => &[],
        }
    }

    /// Whether `self -> next` is a valid lifecycle step. A state is never a
    /// valid transition target of itself.
    pub fn can_transition_to(self, next: SettlementStatus) -> bool {
        self.allowed_next().contains(&next)
    }

    /// Apply a transition, rejecting any step the lifecycle graph disallows.
    pub fn transition(
        self,
        next: SettlementStatus,
    ) -> Result<SettlementStatus, SettlementTransitionError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(SettlementTransitionError { from: self, to: next })
        }
    }
}

/// A rejected `SettlementStatus` transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementTransitionError {
    pub from: SettlementStatus,
    pub to: SettlementStatus,
}

impl std::fmt::Display for SettlementTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid settlement transition: {:?} -> {:?}",
            self.from, self.to
        )
    }
}

impl std::error::Error for SettlementTransitionError {}

/// Per-UTXO inventory entry returned by `RgbBackend::list_inventory_utxos`.
/// `btc_sats` may be 0 when the backend hasn't surfaced bp-wallet UTXO data yet
/// (the value is only used by the maker's fragmentation heuristics, which fall
/// back to amount-based math when sats are unknown).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RgbInventoryUtxo {
    pub outpoint: Outpoint,
    pub asset_id: AssetId,
    pub amount: u64,
    pub btc_sats: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ReservationId(pub String);

/// Lifecycle state of a single RGB-colored UTXO tracked by the maker's
/// inventory store. Maps onto the settlement state machine (#9):
/// `Reserved` → `PendingBitcoinConfirm` (broadcast) → `PendingRgbAcceptance`
/// (confirmed) → `Spent` (counterparty accepted).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum InventoryStatus {
    Available,
    Reserved {
        reservation_id: ReservationId,
        rfq_id: RfqId,
        quote_id: QuoteId,
        expires_at_ms: u64,
    },
    /// Tx is broadcast; awaiting bitcoin confirmation. `reservation_id` is
    /// retained so the settlement state machine (#9) can drive transitions
    /// keyed by reservation rather than scanning by witness_txid.
    PendingBitcoinConfirm {
        reservation_id: ReservationId,
        witness_txid: String,
    },
    /// Tx is bitcoin-confirmed; awaiting RGB consignment acceptance.
    PendingRgbAcceptance {
        reservation_id: ReservationId,
        witness_txid: String,
    },
    Spent {
        witness_txid: String,
        quote_id: QuoteId,
    },
    Invalid {
        reason: String,
    },
}

/// Inventory store row. One per outpoint. Authoritative state for the maker —
/// `RgbInventoryUtxo` is the read-only chain view; this wraps it with lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InventoryUtxo {
    pub outpoint: Outpoint,
    pub asset_id: AssetId,
    pub amount: u64,
    pub btc_sats: u64,
    pub status: InventoryStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Set when this UTXO entered inventory as the change output of a still-
    /// unconfirmed broadcast tx. Cleared once the tx confirms.
    pub pending_txid: Option<String>,
}

/// Snapshot of the maker's inventory health for an asset. Derived from the
/// store on demand; not stored. `f64` fields prevent an `Eq` derive but
/// `PartialEq` is enough for assertions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ExtendedInventorySnapshot {
    pub total_amount: u64,
    pub available_amount: u64,
    pub reserved_amount: u64,
    pub pending_settlement_amount: u64,
    pub spent_amount: u64,
    pub total_utxos: u64,
    pub available_utxos: u64,
    pub reserved_utxos: u64,
    pub pending_settlement_utxos: u64,
    pub spent_utxos: u64,
    pub invalid_utxos: u64,
    pub fragmentation_score: f64,
    pub average_input_count: f64,
    pub average_change_ratio: f64,
    pub pending_settlements: u64,
}

impl From<&ExtendedInventorySnapshot> for InventorySnapshot {
    fn from(ext: &ExtendedInventorySnapshot) -> Self {
        Self {
            total_amount: ext.total_amount,
            available_amount: ext.available_amount,
            reserved_amount: ext.reserved_amount,
            spent_amount: ext.spent_amount,
            total_allocations: ext.total_utxos,
            available_allocations: ext.available_utxos,
            reserved_allocations: ext.reserved_utxos,
            spent_allocations: ext.spent_utxos,
        }
    }
}

impl ExtendedInventorySnapshot {
    /// Compute a fresh snapshot from a UTXO iterator. The aggregation rules are
    /// the spec for `InventoryStore::extended_snapshot` — backends should
    /// either delegate to this or match its output exactly.
    ///
    /// `fragmentation_score = 1.0 - (largest_available / total_available)`,
    /// with a 0.0 fallback when no available amount exists. `average_input_count`
    /// and `average_change_ratio` are rolling settlement metrics fed externally
    /// (not derivable from inventory state) and stay at 0.0 here.
    pub fn from_utxos<'a, I>(utxos: I) -> Self
    where
        I: IntoIterator<Item = &'a InventoryUtxo>,
    {
        let mut snap = ExtendedInventorySnapshot::default();
        let mut largest_available: u64 = 0;

        for utxo in utxos {
            snap.total_amount = snap.total_amount.saturating_add(utxo.amount);
            snap.total_utxos += 1;
            match &utxo.status {
                InventoryStatus::Available => {
                    snap.available_amount = snap.available_amount.saturating_add(utxo.amount);
                    snap.available_utxos += 1;
                    if utxo.amount > largest_available {
                        largest_available = utxo.amount;
                    }
                }
                InventoryStatus::Reserved { .. } => {
                    snap.reserved_amount = snap.reserved_amount.saturating_add(utxo.amount);
                    snap.reserved_utxos += 1;
                    snap.pending_settlements += 1;
                }
                InventoryStatus::PendingBitcoinConfirm { .. }
                | InventoryStatus::PendingRgbAcceptance { .. } => {
                    snap.pending_settlement_amount =
                        snap.pending_settlement_amount.saturating_add(utxo.amount);
                    snap.pending_settlement_utxos += 1;
                    snap.pending_settlements += 1;
                }
                InventoryStatus::Spent { .. } => {
                    snap.spent_amount = snap.spent_amount.saturating_add(utxo.amount);
                    snap.spent_utxos += 1;
                }
                InventoryStatus::Invalid { .. } => {
                    snap.invalid_utxos += 1;
                }
            }
        }

        snap.fragmentation_score = if snap.available_amount > 0 {
            1.0 - (largest_available as f64 / snap.available_amount as f64)
        } else {
            0.0
        };

        snap
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum InventoryError {
    UtxoNotFound(Outpoint),
    UtxoNotAvailable {
        outpoint: Outpoint,
        status: InventoryStatus,
    },
    ReservationNotFound(ReservationId),
    InvalidTransition {
        from: InventoryStatus,
        to: String,
    },
}

impl std::fmt::Display for InventoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UtxoNotFound(op) => write!(f, "utxo not found: {op}"),
            Self::UtxoNotAvailable { outpoint, status } => write!(
                f,
                "utxo {outpoint} is not available (status: {status:?})"
            ),
            Self::ReservationNotFound(id) => write!(f, "reservation not found: {}", id.0),
            Self::InvalidTransition { from, to } => {
                write!(f, "invalid inventory transition: {from:?} -> {to}")
            }
        }
    }
}

impl std::error::Error for InventoryError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct InventorySnapshot {
    pub total_amount: u64,
    pub available_amount: u64,
    pub reserved_amount: u64,
    pub spent_amount: u64,
    pub total_allocations: u64,
    pub available_allocations: u64,
    pub reserved_allocations: u64,
    pub spent_allocations: u64,
}

// --- Maker BTC inventory (issue #20 / 16a) ---
//
// The maker holds plain (non-RGB) BTC UTXOs to pay out on the sell-side swap.
// These types mirror the RGB inventory shapes above — same lifecycle, simpler
// per-row data (sats + scriptPubKey, no asset id or token amount). The RGB
// `PendingRgbAcceptance` stage has no BTC analogue: a plain BTC payout is
// final once the witness tx confirms.

/// Lifecycle state of a single BTC UTXO in the maker's BTC inventory.
/// `Available → Reserved → PendingBitcoinConfirm → Spent`, with `Invalid` for
/// reorg-orphaned outputs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BtcInventoryStatus {
    Available,
    Reserved {
        reservation_id: ReservationId,
        rfq_id: RfqId,
        quote_id: QuoteId,
        expires_at_ms: u64,
    },
    PendingBitcoinConfirm {
        reservation_id: ReservationId,
        witness_txid: String,
    },
    Spent {
        witness_txid: String,
        quote_id: QuoteId,
    },
    Invalid {
        reason: String,
    },
}

/// One BTC UTXO row in the maker's BTC inventory store. `script_pubkey` is
/// retained so PSBT input construction (16c) doesn't have to re-fetch it via
/// `BitcoinClient::get_outpoint`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BtcInventoryUtxo {
    pub outpoint: Outpoint,
    pub value_sats: u64,
    pub script_pubkey: Vec<u8>,
    pub status: BtcInventoryStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Set when this UTXO entered inventory as the change output of a still-
    /// unconfirmed broadcast tx. Cleared once the tx confirms.
    pub pending_txid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct BtcInventorySnapshot {
    pub total_sats: u64,
    pub available_sats: u64,
    pub reserved_sats: u64,
    pub pending_settlement_sats: u64,
    pub spent_sats: u64,
    pub total_utxos: u64,
    pub available_utxos: u64,
    pub reserved_utxos: u64,
    pub pending_settlement_utxos: u64,
    pub spent_utxos: u64,
    pub invalid_utxos: u64,
}

impl BtcInventorySnapshot {
    /// Aggregate a snapshot from a UTXO iterator. The rules here are the spec
    /// for `BtcInventoryStore::snapshot` — backends should delegate to this or
    /// match its output exactly.
    pub fn from_utxos<'a, I>(utxos: I) -> Self
    where
        I: IntoIterator<Item = &'a BtcInventoryUtxo>,
    {
        let mut snap = BtcInventorySnapshot::default();
        for utxo in utxos {
            snap.total_sats = snap.total_sats.saturating_add(utxo.value_sats);
            snap.total_utxos += 1;
            match &utxo.status {
                BtcInventoryStatus::Available => {
                    snap.available_sats = snap.available_sats.saturating_add(utxo.value_sats);
                    snap.available_utxos += 1;
                }
                BtcInventoryStatus::Reserved { .. } => {
                    snap.reserved_sats = snap.reserved_sats.saturating_add(utxo.value_sats);
                    snap.reserved_utxos += 1;
                }
                BtcInventoryStatus::PendingBitcoinConfirm { .. } => {
                    snap.pending_settlement_sats =
                        snap.pending_settlement_sats.saturating_add(utxo.value_sats);
                    snap.pending_settlement_utxos += 1;
                }
                BtcInventoryStatus::Spent { .. } => {
                    snap.spent_sats = snap.spent_sats.saturating_add(utxo.value_sats);
                    snap.spent_utxos += 1;
                }
                BtcInventoryStatus::Invalid { .. } => {
                    snap.invalid_utxos += 1;
                }
            }
        }
        snap
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BtcInventoryError {
    UtxoNotFound(Outpoint),
    UtxoNotAvailable {
        outpoint: Outpoint,
        status: BtcInventoryStatus,
    },
    ReservationNotFound(ReservationId),
    /// A multi-UTXO reservation or coin selection couldn't cover `requested`.
    Insufficient {
        requested: u64,
        available: u64,
    },
}

impl std::fmt::Display for BtcInventoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UtxoNotFound(op) => write!(f, "btc utxo not found: {op}"),
            Self::UtxoNotAvailable { outpoint, status } => write!(
                f,
                "btc utxo {outpoint} is not available (status: {status:?})"
            ),
            Self::ReservationNotFound(id) => {
                write!(f, "btc reservation not found: {}", id.0)
            }
            Self::Insufficient {
                requested,
                available,
            } => write!(
                f,
                "insufficient btc inventory: requested {requested} sats, available {available} sats"
            ),
        }
    }
}

impl std::error::Error for BtcInventoryError {}

/// Half-signed swap PSBT + consignment returned by the maker. The taker
/// validates the consignment (buy side) or has already built it (sell side),
/// signs its inputs, and returns the fully-signed PSBT via `/sign`.
///
/// Wire format: `partial_psbt` and `consignment` are base64-encoded (see
/// `psbt_base64` / `consignment_base64` parameter naming in `rfq-wallet`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapTransfer {
    pub partial_psbt: String,
    /// Maker-built consignment for the RGB leg (buy side). On sell side this
    /// is `None` because the taker built and submitted the consignment via
    /// `/consignment`.
    pub consignment: Option<String>,
    /// Pre-computed witness txid. Known once all inputs are committed —
    /// after `/consignment` on sell side, after `/sign` on buy side.
    pub expected_witness_txid: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    const VALID_TXID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn outpoint_display_uses_txid_colon_vout() {
        let op = Outpoint::new(VALID_TXID, 3);
        assert_eq!(op.to_string(), format!("{VALID_TXID}:3"));
    }

    #[test]
    fn outpoint_from_str_parses_valid_string() {
        let s = format!("{VALID_TXID}:7");
        let op = Outpoint::from_str(&s).unwrap();
        assert_eq!(op.txid, VALID_TXID);
        assert_eq!(op.vout, 7);
    }

    #[test]
    fn outpoint_round_trips_through_display_and_from_str() {
        let original = Outpoint::new(VALID_TXID, 42);
        let parsed = Outpoint::from_str(&original.to_string()).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn outpoint_from_str_rejects_missing_colon() {
        assert!(Outpoint::from_str(VALID_TXID).is_err());
    }

    #[test]
    fn outpoint_from_str_rejects_short_txid() {
        assert!(Outpoint::from_str("abc:0").is_err());
    }

    #[test]
    fn outpoint_from_str_rejects_uppercase_hex() {
        let upper = VALID_TXID.to_uppercase();
        assert!(Outpoint::from_str(&format!("{upper}:0")).is_err());
    }

    #[test]
    fn outpoint_from_str_rejects_non_hex_txid() {
        let bad = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg";
        assert!(Outpoint::from_str(&format!("{bad}:0")).is_err());
    }

    #[test]
    fn outpoint_from_str_rejects_invalid_vout() {
        assert!(Outpoint::from_str(&format!("{VALID_TXID}:notanumber")).is_err());
    }

    #[test]
    fn outpoint_serde_round_trip() {
        let op = Outpoint::new(VALID_TXID, 9);
        let json = serde_json::to_string(&op).unwrap();
        let parsed: Outpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(op, parsed);
    }

    // --- SettlementStatus state machine (#9) ---

    use SettlementStatus::*;

    #[test]
    fn buy_side_lifecycle_is_a_valid_path() {
        // Pending -> AwaitingTakerSignature -> PendingBitcoinConfirm -> Settled
        assert!(Pending.can_transition_to(AwaitingTakerSignature));
        assert!(AwaitingTakerSignature.can_transition_to(PendingBitcoinConfirm));
        assert!(PendingBitcoinConfirm.can_transition_to(Settled));
    }

    #[test]
    fn sell_side_lifecycle_is_a_valid_path() {
        // Pending -> AwaitingConsignment -> AwaitingTakerSignature -> PendingBitcoinConfirm
        assert!(Pending.can_transition_to(AwaitingConsignment));
        assert!(AwaitingConsignment.can_transition_to(AwaitingTakerSignature));
        assert!(AwaitingTakerSignature.can_transition_to(PendingBitcoinConfirm));
    }

    #[test]
    fn any_non_terminal_state_can_fail() {
        for state in [
            Pending,
            Accepted,
            AwaitingConsignment,
            AwaitingTakerSignature,
            PendingBitcoinConfirm,
        ] {
            assert!(state.can_transition_to(Failed), "{state:?} should reach Failed");
        }
    }

    #[test]
    fn stage_skips_and_rewinds_are_rejected() {
        // Skipping the signature stage.
        assert!(!AwaitingConsignment.can_transition_to(PendingBitcoinConfirm));
        // Jumping straight to Settled.
        assert!(!AwaitingTakerSignature.can_transition_to(Settled));
        // Rewinding.
        assert!(!PendingBitcoinConfirm.can_transition_to(AwaitingTakerSignature));
        // Sell stage on the buy path.
        assert!(!AwaitingTakerSignature.can_transition_to(AwaitingConsignment));
    }

    #[test]
    fn a_state_never_transitions_to_itself() {
        for state in [
            Pending,
            Accepted,
            AwaitingConsignment,
            AwaitingTakerSignature,
            PendingBitcoinConfirm,
            Settled,
            Failed,
        ] {
            assert!(!state.can_transition_to(state), "{state:?} -> {state:?}");
        }
    }

    #[test]
    fn terminal_states_have_no_outgoing_transitions() {
        assert!(Settled.is_terminal());
        assert!(Failed.is_terminal());
        assert!(Settled.allowed_next().is_empty());
        assert!(Failed.allowed_next().is_empty());
        assert!(!AwaitingTakerSignature.is_terminal());
    }

    #[test]
    fn transition_returns_next_state_or_rejects() {
        assert_eq!(
            AwaitingTakerSignature.transition(PendingBitcoinConfirm),
            Ok(PendingBitcoinConfirm),
        );
        let err = AwaitingTakerSignature
            .transition(Settled)
            .expect_err("skipping PendingBitcoinConfirm must be rejected");
        assert_eq!(err.from, AwaitingTakerSignature);
        assert_eq!(err.to, Settled);
    }
}
