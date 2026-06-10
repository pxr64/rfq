use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use rfq_types::{
    AssetId, ExtendedInventorySnapshot, InventoryError, InventoryStatus, InventoryUtxo, MakerId,
    Outpoint, Quote, QuoteId, ReservationId, RfqId, Side, SettlementStatus,
};
use tokio::sync::RwLock;
use uuid::Uuid;

mod btc;
pub use btc::{
    BtcCoinSelectionError, BtcCoinSelector, BtcInventoryStore, BtcSelection,
    GreedyLargestFirstSelector, InMemoryBtcInventoryStore,
};

#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "sqlite")]
pub use sqlite::{
    SqliteBtcInventoryStore, SqliteConsignmentStore, SqliteFillStore, SqliteInventoryStore,
    SqliteOrderStore,
};

#[cfg(feature = "postgres")]
mod pg;
#[cfg(feature = "postgres")]
pub use pg::PostgresSettlementStore;

// ---------------------------------------------------------------------------
// Settlement store (broker) — the source for the tx explorer (colorex-dapp#1).
// Records swaps the broker relays, metadata only (no taker identity, no
// consignment blob). The broker uses the Postgres impl; the in-memory impl is the
// default + the spec for tests. See docs/broker-explorer-plan.md.
// ---------------------------------------------------------------------------

/// One swap the broker relayed, as the explorer sees it. Metadata only — the
/// witness txid is already public on-chain; taker identifiers and the consignment
/// blob are deliberately absent (the consignment is the authed stash, #33).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementRecord {
    pub quote_id: QuoteId,
    pub maker_id: MakerId,
    pub base_asset: AssetId,
    pub quote_asset: AssetId,
    pub side: Side,
    pub amount: u64,
    pub price: u64,
    pub fee_sats: u64,
    /// Set once the maker broadcasts (at `/sign`); `None` before then.
    pub witness_txid: Option<String>,
    pub status: SettlementStatus,
    /// Block height once the confirmation loop sees the witness mined.
    pub confirmed_height: Option<u32>,
    /// Median competing-quote price at RFQ time (same unit as `price`) — the
    /// explorer's "mid" for a Δ-vs-mid figure. `None` if the broker didn't record
    /// RFQ stats. Set once (at `/accept`) and preserved across the `/sign` upsert.
    pub mid: Option<u64>,
    /// How many makers quoted this RFQ — the explorer's "best of N". `None` if
    /// unrecorded. Set once and preserved like `mid`.
    pub quote_count: Option<u32>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Filters for `list_settlements`. All `None` → unfiltered (newest first).
#[derive(Debug, Clone, Default)]
pub struct SettlementFilter {
    pub maker_id: Option<MakerId>,
    pub base_asset_id: Option<String>,
    pub quote_asset_id: Option<String>,
    pub side: Option<Side>,
    pub status: Option<SettlementStatus>,
    pub since_ms: Option<u64>,
}

/// Offset pagination (v1). A `(created_at, quote_id)` cursor can replace this later.
#[derive(Debug, Clone, Copy)]
pub struct Page {
    pub limit: u32,
    pub offset: u32,
}

impl Default for Page {
    fn default() -> Self {
        Self { limit: 50, offset: 0 }
    }
}

#[derive(Debug, Clone)]
pub enum SettlementError {
    Backend(String),
}

impl std::fmt::Display for SettlementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(msg) => write!(f, "settlement store error: {msg}"),
        }
    }
}

impl std::error::Error for SettlementError {}

#[async_trait]
pub trait SettlementStore: Send + Sync {
    /// Upsert by `quote_id` (recorded at `/accept`, updated at `/sign`). Preserves
    /// the original `created_at_ms` on conflict; refreshes everything else.
    async fn save_settlement(&self, record: SettlementRecord) -> Result<(), SettlementError>;

    /// Promote a row's status (+ confirmed height), e.g. the confirmation loop
    /// flipping `PendingBitcoinConfirm → Settled`. Returns whether a row matched.
    async fn update_status(
        &self,
        quote_id: &QuoteId,
        status: SettlementStatus,
        confirmed_height: Option<u32>,
        now_ms: u64,
    ) -> Result<bool, SettlementError>;

    async fn get_settlement(&self, quote_id: &QuoteId)
        -> Result<Option<SettlementRecord>, SettlementError>;

    /// Newest-first, filtered + paginated — the explorer read path.
    async fn list_settlements(
        &self,
        filter: &SettlementFilter,
        page: Page,
    ) -> Result<Vec<SettlementRecord>, SettlementError>;

    /// `(quote_id, witness_txid)` for every row still `PendingBitcoinConfirm` with a
    /// witness — the work-list for the broker's confirmation loop.
    async fn pending_witness_txids(&self)
        -> Result<Vec<(QuoteId, String)>, SettlementError>;
}

#[derive(Debug, Clone, Default)]
pub struct InMemorySettlementStore {
    by_quote: Arc<RwLock<HashMap<QuoteId, SettlementRecord>>>,
}

impl InMemorySettlementStore {
    pub fn new() -> Self {
        Self::default()
    }
}

fn matches_filter(r: &SettlementRecord, f: &SettlementFilter) -> bool {
    f.maker_id.as_ref().is_none_or(|m| &r.maker_id == m)
        && f.base_asset_id.as_ref().is_none_or(|a| &r.base_asset.id == a)
        && f.quote_asset_id.as_ref().is_none_or(|a| &r.quote_asset.id == a)
        && f.side.as_ref().is_none_or(|s| &r.side == s)
        && f.status.as_ref().is_none_or(|s| &r.status == s)
        && f.since_ms.is_none_or(|t| r.created_at_ms >= t)
}

#[async_trait]
impl SettlementStore for InMemorySettlementStore {
    async fn save_settlement(&self, mut record: SettlementRecord) -> Result<(), SettlementError> {
        let mut map = self.by_quote.write().await;
        if let Some(existing) = map.get(&record.quote_id) {
            record.created_at_ms = existing.created_at_ms; // upsert keeps first-seen time
            // RFQ stats are recorded once (at /accept); don't clobber them with the
            // None a later /sign upsert carries.
            record.mid = record.mid.or(existing.mid);
            record.quote_count = record.quote_count.or(existing.quote_count);
        }
        map.insert(record.quote_id.clone(), record);
        Ok(())
    }

    async fn update_status(
        &self,
        quote_id: &QuoteId,
        status: SettlementStatus,
        confirmed_height: Option<u32>,
        now_ms: u64,
    ) -> Result<bool, SettlementError> {
        let mut map = self.by_quote.write().await;
        match map.get_mut(quote_id) {
            Some(r) => {
                r.status = status;
                r.confirmed_height = confirmed_height;
                r.updated_at_ms = now_ms;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn get_settlement(
        &self,
        quote_id: &QuoteId,
    ) -> Result<Option<SettlementRecord>, SettlementError> {
        Ok(self.by_quote.read().await.get(quote_id).cloned())
    }

    async fn list_settlements(
        &self,
        filter: &SettlementFilter,
        page: Page,
    ) -> Result<Vec<SettlementRecord>, SettlementError> {
        let map = self.by_quote.read().await;
        let mut rows: Vec<SettlementRecord> =
            map.values().filter(|r| matches_filter(r, filter)).cloned().collect();
        // Newest first; quote_id breaks ties for a stable order.
        rows.sort_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then_with(|| b.quote_id.0.cmp(&a.quote_id.0))
        });
        Ok(rows
            .into_iter()
            .skip(page.offset as usize)
            .take(page.limit as usize)
            .collect())
    }

    async fn pending_witness_txids(&self) -> Result<Vec<(QuoteId, String)>, SettlementError> {
        Ok(self
            .by_quote
            .read()
            .await
            .values()
            .filter(|r| r.status == SettlementStatus::PendingBitcoinConfirm)
            .filter_map(|r| r.witness_txid.clone().map(|t| (r.quote_id.clone(), t)))
            .collect())
    }
}

/// A consignment the maker produced for a settled swap, kept so it can be
/// re-served if the recipient loses theirs (failed delivery, wallet reset). The
/// cheap counterpart to `colorex maker reconsign`, which re-derives from the stash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsignmentRecord {
    pub quote_id: QuoteId,
    /// RGB contract id (the `base_asset` id of the quote).
    pub contract_id: String,
    /// The swap's witness txid the consignment anchors to.
    pub witness_txid: String,
    /// The base64 `final_consignment` handed to the recipient.
    pub consignment: String,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone)]
pub enum ConsignmentError {
    Backend(String),
}

impl std::fmt::Display for ConsignmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(msg) => write!(f, "consignment store error: {msg}"),
        }
    }
}

impl std::error::Error for ConsignmentError {}

/// Durable store of the maker's produced consignments, keyed by quote id.
/// Persisting is best-effort at the call site: a failure must not abort an
/// otherwise-settled swap (the consignment is still returned to the taker).
#[async_trait]
pub trait ConsignmentStore: Send + Sync {
    /// Upsert by `quote_id` (a re-settlement of the same quote replaces).
    async fn save_consignment(&self, record: ConsignmentRecord) -> Result<(), ConsignmentError>;

    async fn get_consignment(&self, quote_id: &QuoteId)
        -> Result<Option<ConsignmentRecord>, ConsignmentError>;

    /// Every record anchored to `witness_txid` (a swap may emit more than one,
    /// e.g. a sell's change consignment).
    async fn get_by_witness(&self, witness_txid: &str)
        -> Result<Vec<ConsignmentRecord>, ConsignmentError>;
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryConsignmentStore {
    by_quote: Arc<RwLock<HashMap<QuoteId, ConsignmentRecord>>>,
}

impl InMemoryConsignmentStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ConsignmentStore for InMemoryConsignmentStore {
    async fn save_consignment(&self, record: ConsignmentRecord) -> Result<(), ConsignmentError> {
        self.by_quote.write().await.insert(record.quote_id.clone(), record);
        Ok(())
    }

    async fn get_consignment(
        &self,
        quote_id: &QuoteId,
    ) -> Result<Option<ConsignmentRecord>, ConsignmentError> {
        Ok(self.by_quote.read().await.get(quote_id).cloned())
    }

    async fn get_by_witness(
        &self,
        witness_txid: &str,
    ) -> Result<Vec<ConsignmentRecord>, ConsignmentError> {
        Ok(self
            .by_quote
            .read()
            .await
            .values()
            .filter(|r| r.witness_txid == witness_txid)
            .cloned()
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Order store — standing maker orders (moved off orders.json into maker.db so
// orders + fills + inventory share one durable store and the `order` CLI can
// write while the daemon reads, via SQLite/WAL).
// ---------------------------------------------------------------------------

/// Canonical lowercase label for a [`Side`] — the form persisted in SQLite and
/// used by the order book / CLI.
pub fn side_str(side: &Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

/// Parse a side label (case-insensitive) into a [`Side`].
pub fn parse_side_str(s: &str) -> Option<Side> {
    match s.to_ascii_lowercase().as_str() {
        "buy" => Some(Side::Buy),
        "sell" => Some(Side::Sell),
        _ => None,
    }
}

/// One standing maker order. `price` is sats per smallest RGB unit; `size` is
/// the largest single quote (smallest RGB units) it backs. `mirror` opts the
/// order into the auto-mirror strategy: on fill, the opposite-side order is
/// auto-upserted at `mirror_spread_bps` off the fill price.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRecord {
    pub id: String,
    /// `"buy"` (taker buys RGB) or `"sell"` (taker sells RGB).
    pub side: String,
    pub asset_id: String,
    pub price: u64,
    pub size: u64,
    pub created_at_ms: u64,
    pub mirror: bool,
    pub mirror_spread_bps: u16,
}

#[derive(Debug, Clone)]
pub enum OrderError {
    Backend(String),
}

impl std::fmt::Display for OrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(msg) => write!(f, "order store error: {msg}"),
        }
    }
}

impl std::error::Error for OrderError {}

/// Durable store of the maker's standing orders — at most one per
/// `(asset_id, side)` (creating a second upserts the first).
#[async_trait]
pub trait OrderStore: Send + Sync {
    async fn list(&self) -> Result<Vec<OrderRecord>, OrderError>;
    /// Insert/replace the order for its `(asset_id, side)`.
    async fn upsert(&self, order: OrderRecord) -> Result<(), OrderError>;
    /// Remove the order with `id`. Returns true if one was removed.
    async fn cancel(&self, id: &str) -> Result<bool, OrderError>;
    async fn get(&self, asset_id: &str, side: &str) -> Result<Option<OrderRecord>, OrderError>;
}

/// `(asset_id, lowercase side)` — the upsert key.
fn order_key(asset_id: &str, side: &str) -> (String, String) {
    (asset_id.to_owned(), side.to_ascii_lowercase())
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryOrderStore {
    by_key: Arc<RwLock<HashMap<(String, String), OrderRecord>>>,
}

impl InMemoryOrderStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl OrderStore for InMemoryOrderStore {
    async fn list(&self) -> Result<Vec<OrderRecord>, OrderError> {
        Ok(self.by_key.read().await.values().cloned().collect())
    }

    async fn upsert(&self, order: OrderRecord) -> Result<(), OrderError> {
        let key = order_key(&order.asset_id, &order.side);
        self.by_key.write().await.insert(key, order);
        Ok(())
    }

    async fn cancel(&self, id: &str) -> Result<bool, OrderError> {
        let mut map = self.by_key.write().await;
        if let Some(key) = map
            .iter()
            .find(|(_, o)| o.id == id)
            .map(|(k, _)| k.clone())
        {
            map.remove(&key);
            return Ok(true);
        }
        Ok(false)
    }

    async fn get(&self, asset_id: &str, side: &str) -> Result<Option<OrderRecord>, OrderError> {
        Ok(self.by_key.read().await.get(&order_key(asset_id, side)).cloned())
    }
}

// ---------------------------------------------------------------------------
// Fill store — one row per settled swap, recorded at broadcast. Feeds the
// `maker inventory` FILLED counter and the auto-mirror strategy.
// ---------------------------------------------------------------------------

/// One settled swap fill, recorded at broadcast. `price` is the TOTAL gross BTC
/// sats of the swap (as on the quote) — divide by `amount` for the per-unit
/// price. `mirrored` flips true once the strategy has placed the mirror order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillRecord {
    pub quote_id: QuoteId,
    pub asset_id: String,
    pub side: Side,
    pub amount: u64,
    pub price: u64,
    pub witness_txid: String,
    pub filled_at_ms: u64,
    pub mirrored: bool,
}

#[derive(Debug, Clone)]
pub enum FillError {
    Backend(String),
}

impl std::fmt::Display for FillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(msg) => write!(f, "fill store error: {msg}"),
        }
    }
}

impl std::error::Error for FillError {}

/// Durable store of settled fills, keyed by `quote_id` (idempotent — a
/// re-submitted `/sign` for the same quote replaces, never double-counts).
#[async_trait]
pub trait FillStore: Send + Sync {
    async fn record_fill(&self, record: FillRecord) -> Result<(), FillError>;
    /// Cumulative `amount` for `(asset_id, side)` with `filled_at_ms >= since_ms`.
    async fn filled_for(
        &self,
        asset_id: &str,
        side: &Side,
        since_ms: u64,
    ) -> Result<u64, FillError>;
    /// Fills not yet mirrored — the strategy loop's work-list.
    async fn list_unmirrored(&self) -> Result<Vec<FillRecord>, FillError>;
    async fn mark_mirrored(&self, quote_id: &QuoteId) -> Result<(), FillError>;
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryFillStore {
    by_quote: Arc<RwLock<HashMap<QuoteId, FillRecord>>>,
}

impl InMemoryFillStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl FillStore for InMemoryFillStore {
    async fn record_fill(&self, record: FillRecord) -> Result<(), FillError> {
        self.by_quote.write().await.insert(record.quote_id.clone(), record);
        Ok(())
    }

    async fn filled_for(
        &self,
        asset_id: &str,
        side: &Side,
        since_ms: u64,
    ) -> Result<u64, FillError> {
        Ok(self
            .by_quote
            .read()
            .await
            .values()
            .filter(|f| f.asset_id == asset_id && &f.side == side && f.filled_at_ms >= since_ms)
            .map(|f| f.amount)
            .sum())
    }

    async fn list_unmirrored(&self) -> Result<Vec<FillRecord>, FillError> {
        Ok(self
            .by_quote
            .read()
            .await
            .values()
            .filter(|f| !f.mirrored)
            .cloned()
            .collect())
    }

    async fn mark_mirrored(&self, quote_id: &QuoteId) -> Result<(), FillError> {
        if let Some(f) = self.by_quote.write().await.get_mut(quote_id) {
            f.mirrored = true;
        }
        Ok(())
    }
}

#[async_trait]
pub trait QuoteStore: Send + Sync {
    async fn save_quote(&self, quote: Quote);

    async fn get_quote(&self, quote_id: &QuoteId) -> Option<Quote>;
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryQuoteStore {
    quotes: Arc<RwLock<HashMap<QuoteId, Quote>>>,
}

impl InMemoryQuoteStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl QuoteStore for InMemoryQuoteStore {
    async fn save_quote(&self, quote: Quote) {
        self.quotes
            .write()
            .await
            .insert(quote.quote_id.clone(), quote);
    }

    async fn get_quote(&self, quote_id: &QuoteId) -> Option<Quote> {
        self.quotes.read().await.get(quote_id).cloned()
    }
}

/// Per-UTXO inventory store. The trait shape admits a SQLite or RocksDB backend
/// in a follow-up; the in-memory impl is the spec for atomicity.
///
/// Mutations that span multiple outpoints (`reserve_utxos`) are all-or-nothing:
/// if any outpoint isn't `Available`, nothing changes. Concurrent callers
/// serialize on a single write lock in the in-memory impl; a SQL backend
/// would map this to a single `BEGIN…COMMIT`.
#[async_trait]
pub trait InventoryStore: Send + Sync {
    /// Replace the entire UTXO set for `asset` with the supplied list. Used
    /// at maker startup after reading from `RgbBackend::list_inventory_utxos`.
    /// UTXOs of other assets are untouched. Idempotent.
    async fn replace_for_asset(
        &self,
        asset: &AssetId,
        utxos: Vec<InventoryUtxo>,
    ) -> Result<(), InventoryError>;

    /// Insert a single change-output UTXO produced by a broadcast tx. Returns
    /// `UtxoNotAvailable` if the outpoint already exists.
    async fn ingest_change_utxo(&self, utxo: InventoryUtxo) -> Result<(), InventoryError>;

    async fn list_for_asset(&self, asset: &AssetId) -> Vec<InventoryUtxo>;

    /// All UTXOs across every asset. Used by the maker for global inventory
    /// summaries that the per-asset surface can't express.
    async fn list_all(&self) -> Vec<InventoryUtxo>;

    /// Subset of `list_for_asset` filtered to `InventoryStatus::Available`.
    /// Input to the coin selector (lands in 14c/14d).
    async fn list_available(&self, asset: &AssetId) -> Vec<InventoryUtxo>;

    async fn get(&self, outpoint: &Outpoint) -> Option<InventoryUtxo>;

    async fn extended_snapshot(&self, asset: &AssetId) -> ExtendedInventorySnapshot;

    /// Locate the active reservation for `quote_id` if one exists. Returns
    /// `None` for unknown / released / already-spent quote ids. Used by
    /// `Maker::accept_quote` to bridge the public `Quote` to the internal
    /// `ReservationId`.
    async fn find_reservation_for_quote(&self, quote_id: &QuoteId) -> Option<ReservationId>;

    /// Atomically reserve every supplied outpoint under a fresh reservation
    /// id. Fails with `UtxoNotAvailable` (and mutates nothing) if any outpoint
    /// is missing or already non-`Available`.
    async fn reserve_utxos(
        &self,
        rfq_id: &RfqId,
        quote_id: &QuoteId,
        outpoints: &[Outpoint],
        expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<ReservationId, InventoryError>;

    /// Release a specific reservation back to `Available`. Returns the count
    /// of UTXOs released.
    async fn release_reservation(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Release every reservation whose `expires_at_ms <= now_ms`. Returns the
    /// count of UTXOs released.
    async fn release_expired_reservations(&self, now_ms: u64) -> usize;

    /// Push out the deadline of an active reservation. Used at accept time:
    /// the quote-stage reservation (`QUOTE_TTL_MS`) is extended to the longer
    /// settlement window (`TAKER_SIGNATURE_TTL_MS`) so the cleanup loop doesn't
    /// release UTXOs out from under an in-flight settlement. Returns the count
    /// of UTXOs whose deadline moved.
    async fn extend_reservation(
        &self,
        reservation_id: &ReservationId,
        new_expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Mark every UTXO in `reservation_id` as `Spent`. Accepts UTXOs currently
    /// in `Reserved`, `PendingBitcoinConfirm`, or `PendingRgbAcceptance` —
    /// the chain-observer loop sweeps `PendingBitcoinConfirm → Spent`
    /// directly once the witness tx confirms, mirroring `BtcInventoryStore`.
    /// Idempotent: a repeat call with the same `reservation_id` succeeds
    /// (no-op) if the UTXOs are already `Spent` under the same witness_txid.
    async fn mark_spent(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Transition every UTXO in `reservation_id` from `Reserved` to
    /// `PendingBitcoinConfirm { witness_txid }`. Used by the settlement
    /// state machine (#9) after the maker broadcasts the witness tx.
    async fn mark_pending_bitcoin_confirm(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Transition every UTXO in `reservation_id` from `PendingBitcoinConfirm`
    /// to `PendingRgbAcceptance`. Used by the settlement state machine (#9)
    /// after the bitcoin tx confirms.
    async fn mark_pending_rgb_acceptance(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Release a reservation back to `Available` because the broadcast itself
    /// failed (mempool rejected, indexer unreachable, etc). Same outcome as
    /// `release_reservation` — distinct method name for observability and
    /// because the settlement state machine (#9) will reach it via a
    /// different state-transition edge than expiry.
    async fn mark_broadcast_failed(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Mark UTXOs in `reservation_id` as `Invalid` because the RGB
    /// consignment was rejected by the counterparty after broadcast.
    /// Input UTXOs cannot be reused — funds are locked behind the witness tx.
    async fn mark_rgb_acceptance_failed(
        &self,
        reservation_id: &ReservationId,
        reason: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Reconcile chain state under a reorg that orphans `witness_txid`:
    /// - Inputs spent into the reorged tx (`Spent { witness_txid: T }`) → `Available`.
    /// - Outputs produced by the reorged tx (`PendingBitcoinConfirm` or
    ///   `PendingRgbAcceptance` referencing T) → `Invalid`.
    ///
    /// Returns the count of UTXOs affected.
    async fn mark_reorged(
        &self,
        witness_txid: &str,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Mark a single UTXO as `Invalid` regardless of its current status. Used
    /// for stash inconsistency detected at startup and any other case where
    /// the maker can't safely treat the UTXO as inventory.
    async fn mark_invalid(
        &self,
        outpoint: &Outpoint,
        reason: String,
        now_ms: u64,
    ) -> Result<(), InventoryError>;
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryInventoryStore {
    utxos: Arc<RwLock<HashMap<Outpoint, InventoryUtxo>>>,
}

impl InMemoryInventoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronous seeding helper. Useful in sync constructors (e.g.
    /// `Maker::new`) where awaiting `replace_for_asset` would force the
    /// whole constructor to become async.
    pub fn with_seed(utxos: Vec<InventoryUtxo>) -> Self {
        let map: HashMap<Outpoint, InventoryUtxo> =
            utxos.into_iter().map(|u| (u.outpoint.clone(), u)).collect();
        Self {
            utxos: Arc::new(RwLock::new(map)),
        }
    }
}

#[async_trait]
impl InventoryStore for InMemoryInventoryStore {
    async fn replace_for_asset(
        &self,
        asset: &AssetId,
        new_utxos: Vec<InventoryUtxo>,
    ) -> Result<(), InventoryError> {
        let mut utxos = self.utxos.write().await;
        utxos.retain(|_, u| &u.asset_id != asset);
        for utxo in new_utxos {
            utxos.insert(utxo.outpoint.clone(), utxo);
        }
        Ok(())
    }

    async fn ingest_change_utxo(&self, utxo: InventoryUtxo) -> Result<(), InventoryError> {
        let mut utxos = self.utxos.write().await;
        if let Some(existing) = utxos.get(&utxo.outpoint) {
            return Err(InventoryError::UtxoNotAvailable {
                outpoint: utxo.outpoint.clone(),
                status: existing.status.clone(),
            });
        }
        utxos.insert(utxo.outpoint.clone(), utxo);
        Ok(())
    }

    async fn list_for_asset(&self, asset: &AssetId) -> Vec<InventoryUtxo> {
        self.utxos
            .read()
            .await
            .values()
            .filter(|u| &u.asset_id == asset)
            .cloned()
            .collect()
    }

    async fn list_all(&self) -> Vec<InventoryUtxo> {
        self.utxos.read().await.values().cloned().collect()
    }

    async fn list_available(&self, asset: &AssetId) -> Vec<InventoryUtxo> {
        self.utxos
            .read()
            .await
            .values()
            .filter(|u| &u.asset_id == asset && matches!(u.status, InventoryStatus::Available))
            .cloned()
            .collect()
    }

    async fn get(&self, outpoint: &Outpoint) -> Option<InventoryUtxo> {
        self.utxos.read().await.get(outpoint).cloned()
    }

    async fn extended_snapshot(&self, asset: &AssetId) -> ExtendedInventorySnapshot {
        let utxos = self.utxos.read().await;
        ExtendedInventorySnapshot::from_utxos(utxos.values().filter(|u| &u.asset_id == asset))
    }

    async fn find_reservation_for_quote(&self, quote_id: &QuoteId) -> Option<ReservationId> {
        self.utxos.read().await.values().find_map(|u| {
            if let InventoryStatus::Reserved {
                reservation_id,
                quote_id: qid,
                ..
            } = &u.status
            {
                if qid == quote_id {
                    return Some(reservation_id.clone());
                }
            }
            None
        })
    }

    async fn reserve_utxos(
        &self,
        rfq_id: &RfqId,
        quote_id: &QuoteId,
        outpoints: &[Outpoint],
        expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<ReservationId, InventoryError> {
        let mut utxos = self.utxos.write().await;

        // Two-pass: validate all outpoints are Available before mutating any.
        for outpoint in outpoints {
            match utxos.get(outpoint) {
                None => return Err(InventoryError::UtxoNotFound(outpoint.clone())),
                Some(u) if !matches!(u.status, InventoryStatus::Available) => {
                    return Err(InventoryError::UtxoNotAvailable {
                        outpoint: outpoint.clone(),
                        status: u.status.clone(),
                    });
                }
                _ => {}
            }
        }

        let reservation_id = ReservationId(Uuid::new_v4().to_string());
        for outpoint in outpoints {
            let utxo = utxos
                .get_mut(outpoint)
                .expect("checked in validation pass above");
            utxo.status = InventoryStatus::Reserved {
                reservation_id: reservation_id.clone(),
                rfq_id: rfq_id.clone(),
                quote_id: quote_id.clone(),
                expires_at_ms,
            };
            utxo.updated_at_ms = now_ms;
        }
        Ok(reservation_id)
    }

    async fn release_reservation(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut released = 0;
        let mut matched_any = false;

        for utxo in utxos.values_mut() {
            if let InventoryStatus::Reserved {
                reservation_id: rid,
                ..
            } = &utxo.status
            {
                if rid == reservation_id {
                    matched_any = true;
                    utxo.status = InventoryStatus::Available;
                    utxo.updated_at_ms = now_ms;
                    released += 1;
                }
            }
        }

        if !matched_any {
            return Err(InventoryError::ReservationNotFound(reservation_id.clone()));
        }
        Ok(released)
    }

    async fn release_expired_reservations(&self, now_ms: u64) -> usize {
        let mut utxos = self.utxos.write().await;
        let mut released = 0;
        for utxo in utxos.values_mut() {
            let expired = matches!(
                &utxo.status,
                InventoryStatus::Reserved { expires_at_ms, .. } if *expires_at_ms <= now_ms
            );
            if expired {
                utxo.status = InventoryStatus::Available;
                utxo.updated_at_ms = now_ms;
                released += 1;
            }
        }
        released
    }

    async fn extend_reservation(
        &self,
        reservation_id: &ReservationId,
        new_expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        for utxo in utxos.values_mut() {
            if let InventoryStatus::Reserved {
                reservation_id: rid,
                rfq_id,
                quote_id,
                ..
            } = &utxo.status
            {
                if rid == reservation_id {
                    utxo.status = InventoryStatus::Reserved {
                        reservation_id: rid.clone(),
                        rfq_id: rfq_id.clone(),
                        quote_id: quote_id.clone(),
                        expires_at_ms: new_expires_at_ms,
                    };
                    utxo.updated_at_ms = now_ms;
                    updated += 1;
                }
            }
        }
        if updated == 0 {
            return Err(InventoryError::ReservationNotFound(reservation_id.clone()));
        }
        Ok(updated)
    }

    async fn mark_spent(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        let mut matched_any = false;
        let mut already_spent = 0;

        for utxo in utxos.values_mut() {
            let (matches, quote_id) = match &utxo.status {
                InventoryStatus::Reserved {
                    reservation_id: rid,
                    quote_id,
                    ..
                } if rid == reservation_id => (true, Some(quote_id.clone())),
                InventoryStatus::PendingBitcoinConfirm {
                    reservation_id: rid,
                    ..
                } if rid == reservation_id => {
                    // Chain-observer sweeps PendingBitcoinConfirm → Spent
                    // directly once the witness tx confirms. Quote id isn't
                    // retained past Reserved; synthesize a placeholder
                    // (same as the PendingRgbAcceptance branch below).
                    (true, None)
                }
                InventoryStatus::PendingRgbAcceptance {
                    reservation_id: rid,
                    ..
                } if rid == reservation_id => {
                    // Quote id is recoverable from the original Reserved state
                    // but not retained on PendingRgbAcceptance. For #9-driven
                    // flows we'd need a side index; #14e callers don't reach
                    // this branch yet, so synthesize a placeholder.
                    (true, None)
                }
                InventoryStatus::Spent {
                    witness_txid: existing_txid,
                    ..
                } if existing_txid == &witness_txid => {
                    already_spent += 1;
                    (false, None)
                }
                _ => (false, None),
            };
            if !matches {
                continue;
            }
            matched_any = true;
            utxo.status = InventoryStatus::Spent {
                witness_txid: witness_txid.clone(),
                quote_id: quote_id
                    .unwrap_or_else(|| QuoteId(format!("recovered-from-{}", reservation_id.0))),
            };
            utxo.updated_at_ms = now_ms;
            updated += 1;
        }

        if !matched_any && already_spent == 0 {
            return Err(InventoryError::ReservationNotFound(reservation_id.clone()));
        }
        Ok(updated)
    }

    async fn mark_pending_bitcoin_confirm(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        for utxo in utxos.values_mut() {
            if let InventoryStatus::Reserved {
                reservation_id: rid,
                ..
            } = &utxo.status
            {
                if rid == reservation_id {
                    utxo.status = InventoryStatus::PendingBitcoinConfirm {
                        reservation_id: reservation_id.clone(),
                        witness_txid: witness_txid.clone(),
                    };
                    utxo.updated_at_ms = now_ms;
                    updated += 1;
                }
            }
        }
        if updated == 0 {
            return Err(InventoryError::ReservationNotFound(reservation_id.clone()));
        }
        Ok(updated)
    }

    async fn mark_pending_rgb_acceptance(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        for utxo in utxos.values_mut() {
            if let InventoryStatus::PendingBitcoinConfirm {
                reservation_id: rid,
                witness_txid,
            } = &utxo.status
            {
                if rid == reservation_id {
                    utxo.status = InventoryStatus::PendingRgbAcceptance {
                        reservation_id: reservation_id.clone(),
                        witness_txid: witness_txid.clone(),
                    };
                    utxo.updated_at_ms = now_ms;
                    updated += 1;
                }
            }
        }
        if updated == 0 {
            return Err(InventoryError::ReservationNotFound(reservation_id.clone()));
        }
        Ok(updated)
    }

    async fn mark_broadcast_failed(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        self.release_reservation(reservation_id, now_ms).await
    }

    async fn mark_rgb_acceptance_failed(
        &self,
        reservation_id: &ReservationId,
        reason: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        for utxo in utxos.values_mut() {
            if let InventoryStatus::PendingRgbAcceptance {
                reservation_id: rid,
                ..
            } = &utxo.status
            {
                if rid == reservation_id {
                    utxo.status = InventoryStatus::Invalid {
                        reason: reason.clone(),
                    };
                    utxo.updated_at_ms = now_ms;
                    updated += 1;
                }
            }
        }
        if updated == 0 {
            return Err(InventoryError::ReservationNotFound(reservation_id.clone()));
        }
        Ok(updated)
    }

    async fn mark_reorged(
        &self,
        witness_txid: &str,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        for utxo in utxos.values_mut() {
            let new_status = match &utxo.status {
                InventoryStatus::Spent {
                    witness_txid: wt, ..
                } if wt == witness_txid => Some(InventoryStatus::Available),
                InventoryStatus::PendingBitcoinConfirm {
                    witness_txid: wt, ..
                }
                | InventoryStatus::PendingRgbAcceptance {
                    witness_txid: wt, ..
                } if wt == witness_txid => Some(InventoryStatus::Invalid {
                    reason: format!("reorged: witness tx {witness_txid} orphaned"),
                }),
                _ => None,
            };
            if let Some(status) = new_status {
                utxo.status = status;
                utxo.updated_at_ms = now_ms;
                updated += 1;
            }
        }
        Ok(updated)
    }

    async fn mark_invalid(
        &self,
        outpoint: &Outpoint,
        reason: String,
        now_ms: u64,
    ) -> Result<(), InventoryError> {
        let mut utxos = self.utxos.write().await;
        let utxo = utxos
            .get_mut(outpoint)
            .ok_or_else(|| InventoryError::UtxoNotFound(outpoint.clone()))?;
        utxo.status = InventoryStatus::Invalid { reason };
        utxo.updated_at_ms = now_ms;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_types::{AssetKind, BitcoinNetwork};

    const NOW_MS: u64 = 1_700_000_000_000;
    const TXID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-test".to_owned(),
        }
    }

    fn other_asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-other".to_owned(),
        }
    }

    fn outpoint(vout: u32) -> Outpoint {
        Outpoint::new(TXID, vout)
    }

    fn utxo(asset_id: AssetId, vout: u32, amount: u64) -> InventoryUtxo {
        InventoryUtxo {
            outpoint: outpoint(vout),
            asset_id,
            amount,
            btc_sats: 1000,
            status: InventoryStatus::Available,
            created_at_ms: NOW_MS,
            updated_at_ms: NOW_MS,
            pending_txid: None,
        }
    }

    async fn seeded_store(utxos: Vec<InventoryUtxo>) -> InMemoryInventoryStore {
        let store = InMemoryInventoryStore::new();
        // Group by asset so replace_for_asset doesn't clobber across assets.
        let mut by_asset: HashMap<AssetId, Vec<InventoryUtxo>> = HashMap::new();
        for u in utxos {
            by_asset.entry(u.asset_id.clone()).or_default().push(u);
        }
        for (asset, set) in by_asset {
            store.replace_for_asset(&asset, set).await.unwrap();
        }
        store
    }

    #[tokio::test]
    async fn list_and_get_round_trip_seeded_utxos() {
        let store = seeded_store(vec![
            utxo(asset(), 0, 100),
            utxo(asset(), 1, 200),
            utxo(other_asset(), 2, 50),
        ])
        .await;

        let mine = store.list_for_asset(&asset()).await;
        assert_eq!(mine.len(), 2);

        let got = store.get(&outpoint(0)).await.unwrap();
        assert_eq!(got.amount, 100);
        assert!(store.get(&outpoint(99)).await.is_none());
    }

    #[tokio::test]
    async fn replace_for_asset_keeps_other_assets_intact() {
        let store = seeded_store(vec![
            utxo(asset(), 0, 100),
            utxo(other_asset(), 1, 999),
        ])
        .await;

        store
            .replace_for_asset(&asset(), vec![utxo(asset(), 2, 250)])
            .await
            .unwrap();

        let mine = store.list_for_asset(&asset()).await;
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].amount, 250);

        let other = store.list_for_asset(&other_asset()).await;
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].amount, 999);
    }

    #[tokio::test]
    async fn reserve_utxos_atomically_or_rejects_with_no_mutation() {
        let store = seeded_store(vec![utxo(asset(), 0, 100), utxo(asset(), 1, 200)]).await;

        // Pre-reserve outpoint(1) under a different reservation.
        let _first = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(1)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        // Second call tries to reserve {0, 1}. Should fail and leave 0 untouched.
        let err = store
            .reserve_utxos(
                &RfqId("rfq-2".into()),
                &QuoteId("q-2".into()),
                &[outpoint(0), outpoint(1)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, InventoryError::UtxoNotAvailable { .. }));
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Available
        ));
    }

    #[tokio::test]
    async fn reserve_utxos_rejects_missing_outpoint() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
        let err = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0), outpoint(42)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, InventoryError::UtxoNotFound(_)));
        // outpoint(0) must remain untouched
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Available
        ));
    }

    #[tokio::test]
    async fn release_expired_reservations_releases_only_past_deadlines() {
        let store = seeded_store(vec![utxo(asset(), 0, 100), utxo(asset(), 1, 200)]).await;
        store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS - 1,
                NOW_MS - 2,
            )
            .await
            .unwrap();
        store
            .reserve_utxos(
                &RfqId("rfq-2".into()),
                &QuoteId("q-2".into()),
                &[outpoint(1)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        let released = store.release_expired_reservations(NOW_MS).await;

        assert_eq!(released, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Available
        ));
        assert!(matches!(
            store.get(&outpoint(1)).await.unwrap().status,
            InventoryStatus::Reserved { .. }
        ));
    }

    #[tokio::test]
    async fn release_reservation_targets_specific_reservation_id() {
        let store = seeded_store(vec![utxo(asset(), 0, 100), utxo(asset(), 1, 200)]).await;
        let rid1 = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();
        let _rid2 = store
            .reserve_utxos(
                &RfqId("rfq-2".into()),
                &QuoteId("q-2".into()),
                &[outpoint(1)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        let count = store.release_reservation(&rid1, NOW_MS).await.unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Available
        ));
        assert!(matches!(
            store.get(&outpoint(1)).await.unwrap().status,
            InventoryStatus::Reserved { .. }
        ));
    }

    #[tokio::test]
    async fn release_reservation_unknown_id_errors() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
        let err = store
            .release_reservation(&ReservationId("does-not-exist".into()), NOW_MS)
            .await
            .unwrap_err();
        assert!(matches!(err, InventoryError::ReservationNotFound(_)));
    }

    #[tokio::test]
    async fn mark_spent_transitions_reservation_to_spent() {
        let store = seeded_store(vec![utxo(asset(), 0, 100), utxo(asset(), 1, 200)]).await;
        let rid = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0), outpoint(1)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        let count = store
            .mark_spent(&rid, "wt-1".into(), NOW_MS + 100)
            .await
            .unwrap();
        assert_eq!(count, 2);

        for vout in [0, 1] {
            assert!(matches!(
                store.get(&outpoint(vout)).await.unwrap().status,
                InventoryStatus::Spent { ref witness_txid, .. } if witness_txid == "wt-1"
            ));
        }
    }

    #[tokio::test]
    async fn mark_spent_idempotent_for_same_witness_txid() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
        let rid = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        let first = store
            .mark_spent(&rid, "wt-1".into(), NOW_MS + 100)
            .await
            .unwrap();
        let second = store
            .mark_spent(&rid, "wt-1".into(), NOW_MS + 200)
            .await
            .unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0, "idempotent repeat returns 0 updates");
    }

    #[tokio::test]
    async fn ingest_change_utxo_inserts_when_new_and_errors_when_duplicate() {
        let store = seeded_store(vec![]).await;
        let change = utxo(asset(), 5, 42);

        store.ingest_change_utxo(change.clone()).await.unwrap();
        assert_eq!(store.get(&outpoint(5)).await.unwrap().amount, 42);

        let err = store.ingest_change_utxo(change).await.unwrap_err();
        assert!(matches!(err, InventoryError::UtxoNotAvailable { .. }));
    }

    #[tokio::test]
    async fn extended_snapshot_aggregates_per_state() {
        let store = seeded_store(vec![
            utxo(asset(), 0, 100),
            utxo(asset(), 1, 200),
            utxo(asset(), 2, 300),
        ])
        .await;
        // Reserve one, leave two available.
        store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        let snap = store.extended_snapshot(&asset()).await;
        assert_eq!(snap.total_amount, 600);
        assert_eq!(snap.available_amount, 500);
        assert_eq!(snap.reserved_amount, 100);
        assert_eq!(snap.total_utxos, 3);
        assert_eq!(snap.available_utxos, 2);
        assert_eq!(snap.reserved_utxos, 1);
        assert_eq!(snap.pending_settlements, 1);
        // largest available is 300 / total available 500 → fragmentation 0.4
        assert!((snap.fragmentation_score - 0.4).abs() < 1e-9);
    }

    #[tokio::test]
    async fn fragmentation_score_is_zero_for_single_available_utxo() {
        let store = seeded_store(vec![utxo(asset(), 0, 1000)]).await;
        let snap = store.extended_snapshot(&asset()).await;
        assert_eq!(snap.fragmentation_score, 0.0);
    }

    #[tokio::test]
    async fn fragmentation_score_zero_when_no_available_amount() {
        let store = seeded_store(vec![]).await;
        let snap = store.extended_snapshot(&asset()).await;
        assert_eq!(snap.fragmentation_score, 0.0);
        assert_eq!(snap.total_amount, 0);
    }

    // --- 14e failure-handling tests ---

    #[tokio::test]
    async fn mark_pending_bitcoin_confirm_transitions_reserved_to_pending() {
        let store = seeded_store(vec![utxo(asset(), 0, 100), utxo(asset(), 1, 200)]).await;
        let rid = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0), outpoint(1)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        let count = store
            .mark_pending_bitcoin_confirm(&rid, "wt-1".into(), NOW_MS + 100)
            .await
            .unwrap();
        assert_eq!(count, 2);

        for vout in [0, 1] {
            let status = store.get(&outpoint(vout)).await.unwrap().status;
            match status {
                InventoryStatus::PendingBitcoinConfirm {
                    reservation_id,
                    witness_txid,
                } => {
                    assert_eq!(reservation_id, rid);
                    assert_eq!(witness_txid, "wt-1");
                }
                other => panic!("expected PendingBitcoinConfirm, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn mark_pending_rgb_acceptance_transitions_pending_btc_to_pending_rgb() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
        let rid = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();
        store
            .mark_pending_bitcoin_confirm(&rid, "wt-1".into(), NOW_MS + 100)
            .await
            .unwrap();

        let count = store
            .mark_pending_rgb_acceptance(&rid, NOW_MS + 200)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::PendingRgbAcceptance { ref witness_txid, .. } if witness_txid == "wt-1"
        ));
    }

    #[tokio::test]
    async fn mark_spent_accepts_pending_bitcoin_confirm_input() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
        let rid = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();
        store
            .mark_pending_bitcoin_confirm(&rid, "wt-1".into(), NOW_MS + 100)
            .await
            .unwrap();

        let count = store
            .mark_spent(&rid, "wt-1".into(), NOW_MS + 200)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Spent { ref witness_txid, .. } if witness_txid == "wt-1"
        ));
    }

    #[tokio::test]
    async fn mark_spent_accepts_pending_rgb_acceptance_input() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
        let rid = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();
        store
            .mark_pending_bitcoin_confirm(&rid, "wt-1".into(), NOW_MS + 100)
            .await
            .unwrap();
        store
            .mark_pending_rgb_acceptance(&rid, NOW_MS + 200)
            .await
            .unwrap();

        let count = store
            .mark_spent(&rid, "wt-1".into(), NOW_MS + 300)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Spent { ref witness_txid, .. } if witness_txid == "wt-1"
        ));
    }

    #[tokio::test]
    async fn mark_broadcast_failed_returns_to_available() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
        let rid = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        let count = store
            .mark_broadcast_failed(&rid, NOW_MS + 100)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Available
        ));
    }

    #[tokio::test]
    async fn mark_rgb_acceptance_failed_transitions_to_invalid_with_reason() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
        let rid = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();
        store
            .mark_pending_bitcoin_confirm(&rid, "wt-1".into(), NOW_MS + 100)
            .await
            .unwrap();
        store
            .mark_pending_rgb_acceptance(&rid, NOW_MS + 200)
            .await
            .unwrap();

        let count = store
            .mark_rgb_acceptance_failed(&rid, "counterparty rejected".into(), NOW_MS + 300)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Invalid { ref reason } if reason == "counterparty rejected"
        ));
    }

    #[tokio::test]
    async fn mark_reorged_releases_spent_inputs_and_invalidates_pending_outputs() {
        let store = seeded_store(vec![
            utxo(asset(), 0, 100),
            utxo(asset(), 1, 50),
            utxo(asset(), 2, 200),
        ])
        .await;
        // UTXO 0: spent in the reorged tx → should go back to Available.
        let rid_spent = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();
        store
            .mark_spent(&rid_spent, "wt-reorged".into(), NOW_MS + 100)
            .await
            .unwrap();
        // UTXO 1: an output (change-style) of the reorged tx that was awaiting
        // bitcoin confirmation → should go to Invalid.
        store
            .ingest_change_utxo(InventoryUtxo {
                status: InventoryStatus::PendingBitcoinConfirm {
                    reservation_id: ReservationId("rid-x".into()),
                    witness_txid: "wt-reorged".into(),
                },
                pending_txid: Some("wt-reorged".into()),
                ..utxo(asset(), 5, 25)
            })
            .await
            .unwrap();
        // UTXO 2: unrelated; should not be touched.

        let updated = store.mark_reorged("wt-reorged", NOW_MS + 200).await.unwrap();
        assert_eq!(updated, 2);

        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Available
        ));
        assert!(matches!(
            store.get(&outpoint(5)).await.unwrap().status,
            InventoryStatus::Invalid { .. }
        ));
        assert!(matches!(
            store.get(&outpoint(2)).await.unwrap().status,
            InventoryStatus::Available
        ));
    }

    #[tokio::test]
    async fn mark_invalid_sets_status_regardless_of_prior_state() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
        store
            .mark_invalid(&outpoint(0), "stash drift".into(), NOW_MS + 100)
            .await
            .unwrap();
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Invalid { ref reason } if reason == "stash drift"
        ));
    }

    #[tokio::test]
    async fn mark_invalid_unknown_outpoint_errors() {
        let store = seeded_store(vec![]).await;
        let err = store
            .mark_invalid(&outpoint(99), "x".into(), NOW_MS)
            .await
            .unwrap_err();
        assert!(matches!(err, InventoryError::UtxoNotFound(_)));
    }

    #[tokio::test]
    async fn legacy_snapshot_view_derives_from_extended_snapshot() {
        let store = seeded_store(vec![
            utxo(asset(), 0, 100),
            utxo(asset(), 1, 200),
        ])
        .await;
        store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        let ext = store.extended_snapshot(&asset()).await;
        let legacy: rfq_types::InventorySnapshot = (&ext).into();
        assert_eq!(legacy.total_amount, 300);
        assert_eq!(legacy.available_amount, 200);
        assert_eq!(legacy.reserved_amount, 100);
        assert_eq!(legacy.spent_amount, 0);
        assert_eq!(legacy.total_allocations, 2);
        assert_eq!(legacy.available_allocations, 1);
        assert_eq!(legacy.reserved_allocations, 1);
        assert_eq!(legacy.spent_allocations, 0);
    }
}

#[cfg(test)]
mod settlement_tests {
    use super::*;
    use rfq_types::{AssetId, AssetKind, BitcoinNetwork, MakerId, QuoteId, Side, SettlementStatus};

    fn asset(id: &str, kind: AssetKind) -> AssetId {
        AssetId { network: BitcoinNetwork::Regtest, kind, id: id.to_owned() }
    }

    fn record(quote: &str, maker: &str, created: u64) -> SettlementRecord {
        SettlementRecord {
            quote_id: QuoteId(quote.to_owned()),
            maker_id: MakerId(maker.to_owned()),
            base_asset: asset("btc", AssetKind::Btc),
            quote_asset: asset("rgb:AAA", AssetKind::Rgb20),
            side: Side::Buy,
            amount: 1000,
            price: 101,
            fee_sats: 200,
            witness_txid: None,
            status: SettlementStatus::Accepted,
            confirmed_height: None,
            mid: None,
            quote_count: None,
            created_at_ms: created,
            updated_at_ms: created,
        }
    }

    #[tokio::test]
    async fn save_get_and_upsert_preserves_created_at_and_rfq_stats() {
        let store = InMemorySettlementStore::new();
        // First record (at /accept) carries the RFQ stats.
        let mut first = record("q1", "m1", 100);
        first.mid = Some(99);
        first.quote_count = Some(4);
        store.save_settlement(first).await.unwrap();

        // Upsert (the /sign update): advance to broadcast; mid/quote_count come as
        // None but must survive, and created_at stays.
        let mut adv = record("q1", "m1", 999);
        adv.status = SettlementStatus::PendingBitcoinConfirm;
        adv.witness_txid = Some("wt1".into());
        store.save_settlement(adv).await.unwrap();

        let got = store.get_settlement(&QuoteId("q1".into())).await.unwrap().unwrap();
        assert_eq!(got.created_at_ms, 100, "created_at preserved across upsert");
        assert_eq!(got.mid, Some(99), "mid preserved across upsert");
        assert_eq!(got.quote_count, Some(4), "quote_count preserved across upsert");
        assert_eq!(got.witness_txid.as_deref(), Some("wt1"));
        assert_eq!(got.status, SettlementStatus::PendingBitcoinConfirm);
    }

    #[tokio::test]
    async fn update_status_promotes_and_reports_match() {
        let store = InMemorySettlementStore::new();
        let mut r = record("q1", "m1", 100);
        r.status = SettlementStatus::PendingBitcoinConfirm;
        r.witness_txid = Some("wt1".into());
        store.save_settlement(r).await.unwrap();

        assert!(store
            .update_status(&QuoteId("q1".into()), SettlementStatus::Settled, Some(142), 200)
            .await
            .unwrap());
        assert!(!store
            .update_status(&QuoteId("missing".into()), SettlementStatus::Settled, None, 200)
            .await
            .unwrap());
        let got = store.get_settlement(&QuoteId("q1".into())).await.unwrap().unwrap();
        assert_eq!(got.status, SettlementStatus::Settled);
        assert_eq!(got.confirmed_height, Some(142));
        assert_eq!(got.updated_at_ms, 200);
    }

    #[tokio::test]
    async fn list_filters_orders_newest_first_and_paginates() {
        let store = InMemorySettlementStore::new();
        store.save_settlement(record("q1", "m1", 100)).await.unwrap();
        store.save_settlement(record("q2", "m2", 300)).await.unwrap();
        store.save_settlement(record("q3", "m1", 200)).await.unwrap();

        let all = store
            .list_settlements(&SettlementFilter::default(), Page::default())
            .await
            .unwrap();
        assert_eq!(
            all.iter().map(|r| r.quote_id.0.clone()).collect::<Vec<_>>(),
            ["q2", "q3", "q1"],
            "newest first"
        );

        let m1 = store
            .list_settlements(
                &SettlementFilter { maker_id: Some(MakerId("m1".into())), ..Default::default() },
                Page::default(),
            )
            .await
            .unwrap();
        assert_eq!(m1.len(), 2);
        assert!(m1.iter().all(|r| r.maker_id.0 == "m1"));

        let recent = store
            .list_settlements(
                &SettlementFilter { since_ms: Some(250), ..Default::default() },
                Page::default(),
            )
            .await
            .unwrap();
        assert_eq!(recent.iter().map(|r| r.quote_id.0.clone()).collect::<Vec<_>>(), ["q2"]);

        let page = store
            .list_settlements(&SettlementFilter::default(), Page { limit: 1, offset: 1 })
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].quote_id.0, "q3");
    }

    #[tokio::test]
    async fn pending_witness_txids_lists_only_broadcast_with_witness() {
        let store = InMemorySettlementStore::new();
        store.save_settlement(record("q1", "m1", 100)).await.unwrap(); // Accepted, no witness
        let mut r2 = record("q2", "m1", 200);
        r2.status = SettlementStatus::PendingBitcoinConfirm;
        r2.witness_txid = Some("wt2".into());
        store.save_settlement(r2).await.unwrap();
        let mut r3 = record("q3", "m1", 300);
        r3.status = SettlementStatus::Settled; // confirmed already — not pending
        r3.witness_txid = Some("wt3".into());
        store.save_settlement(r3).await.unwrap();

        let pending = store.pending_witness_txids().await.unwrap();
        assert_eq!(pending, vec![(QuoteId("q2".into()), "wt2".to_owned())]);
    }
}
