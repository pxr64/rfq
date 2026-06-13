use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use rfq_btc::BitcoinClient;
use rfq_rgb::{ContractId, RgbBackend, TxOut};
use rfq_router::{MakerConnector, RouterError};
use rfq_store::{
    BtcCoinSelector, BtcInventoryStore, ConsignmentRecord, ConsignmentStore, FillRecord, FillStore,
    GreedyLargestFirstSelector, InMemoryBtcInventoryStore, InMemoryConsignmentStore,
    InMemoryFillStore, InMemoryInventoryStore, InventoryStore,
};
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetInfo, BitcoinNetwork, BtcInventoryStatus, BtcInventoryUtxo,
    ExtendedInventorySnapshot, InventoryError, InventorySnapshot, InventoryStatus, InventoryUtxo,
    MakerId, OrderPrice, Outpoint, Quote, QuoteId, QuoteRequest, ReservationId, RfqId,
    RgbInventoryUtxo, SettlementIntent, SettlementStatus, Side, SwapLeg,
};
use std::str::FromStr as _;
use tokio::sync::RwLock;
use uuid::Uuid;

mod coin_select;
pub use coin_select::{CoinSelectionError, CoinSelector, GreedyExactFitSelector, Selection};

mod rebalance;
pub use rebalance::*;

/// Standing per-(asset, side) price the maker quotes at. Built from the operator's
/// saved orders (`colorex maker order ...`); empty by default, so a maker with no
/// orders quotes nothing — it declines until an operator prices the (asset, side).
/// Linear-scanned — an operator runs a handful of
/// orders, not thousands.
#[derive(Debug, Clone, Default)]
pub struct PricePolicy {
    entries: Vec<PriceEntry>,
}

/// One standing order's pricing terms: a unit price (sats per smallest RGB
/// unit) and the largest single-quote amount it backs.
#[derive(Debug, Clone)]
pub struct PriceEntry {
    pub asset_id: String,
    pub side: Side,
    pub price_sats_per_unit: u64,
    pub max_size: u64,
}

/// Outcome of consulting the [`PricePolicy`] for a quote.
pub enum PriceLookup {
    /// Quote at this unit price (sats per smallest RGB unit).
    Price(u64),
    /// A matching order exists but the requested amount exceeds its size —
    /// decline the quote.
    Decline,
    /// No standing order for this (asset, side) — decline. The maker quotes only
    /// what an operator has explicitly priced; there is no flat fallback.
    NoOrder,
}

impl PricePolicy {
    pub fn from_entries(entries: Vec<PriceEntry>) -> Self {
        Self { entries }
    }

    pub fn entries(&self) -> &[PriceEntry] {
        &self.entries
    }

    /// Resolve the unit price for a quote of `amount` on (`asset`, `side`).
    pub fn unit_price(&self, asset: &AssetId, side: &Side, amount: u64) -> PriceLookup {
        match self
            .entries
            .iter()
            .find(|e| e.asset_id == asset.id && &e.side == side)
        {
            None => PriceLookup::NoOrder,
            Some(e) if amount <= e.max_size => PriceLookup::Price(e.price_sats_per_unit),
            Some(_) => PriceLookup::Decline,
        }
    }
}

/// Inputs to the periodic rebalance planner. Defaults are conservative — the
/// loop should fire rarely under normal operation. See
/// `docs/rebalancing-strategy.md` for the why behind these thresholds.
#[derive(Debug, Clone)]
pub struct RebalancePolicy {
    pub fragmentation_threshold: f64,
    pub max_utxo_count: u64,
    pub min_utxo_count: u64,
}

impl Default for RebalancePolicy {
    fn default() -> Self {
        Self {
            fragmentation_threshold: 0.7,
            max_utxo_count: 50,
            min_utxo_count: 3,
        }
    }
}

/// Proposed rebalance actions plus the trigger reasons that fired. In 14e
/// `merges` and `splits` are always empty placeholders — execution is deferred
/// to the follow-up issue. The trigger list is what the loop logs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RebalancePlan {
    pub triggers: Vec<RebalanceTrigger>,
    pub merges: Vec<MergeAction>,
    pub splits: Vec<SplitAction>,
}

impl RebalancePlan {
    pub fn is_empty(&self) -> bool {
        self.triggers.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RebalanceTrigger {
    HighFragmentation { score: f64, threshold: f64 },
    TooManyUtxos { count: u64, max: u64 },
    TooFewUtxos { count: u64, min: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MergeAction {
    pub asset: AssetId,
    pub inputs: Vec<Outpoint>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SplitAction {
    pub asset: AssetId,
    pub input: Outpoint,
    pub target_pieces: u32,
}

const QUOTE_TTL_MS: u64 = 30_000;
/// After the taker accepts, the maker holds the reservation this long awaiting
/// the signed PSBT via `/sign` — longer than `QUOTE_TTL_MS` because the taker
/// now has to add BTC inputs and sign. See `docs/swap-flows.md`.
const TAKER_SIGNATURE_TTL_MS: u64 = 600_000;
/// After broadcast, the reservation sits in `PendingBitcoinConfirm` until the
/// witness tx confirms (or a reorg / timeout intervenes).
const BROADCAST_CONFIRM_TTL_MS: u64 = 7_200_000;
/// Rough vbyte footprint of a 2-in / 3-out segwit swap tx. Multiplied by the
/// feerate estimate to turn it into an absolute fee on the quote. A real
/// estimate would size the actual PSBT; this is a deliberate v0 placeholder.
const ESTIMATED_SWAP_VBYTES: u64 = 200;
/// Floor for the swap feerate (sat/vByte). Low-activity networks (regtest,
/// signet) return no electrum estimate, which would yield a 0-fee tx that
/// bitcoind rejects under `minrelaytxfee`. 1 sat/vByte clears the default floor.
const MIN_SWAP_FEERATE_SAT_VB: u64 = 1;
/// Mainnet ceiling on the swap feerate (sat/vByte) — a sanity guard so a wildly
/// wrong electrum estimate can't inflate the fee to absurd levels.
const MAX_SWAP_FEERATE_SAT_VB: u64 = 1000;
/// Ceiling for TEST networks (signet / testnet / regtest). `estimatesmartfee` is
/// unreliable on low-activity chains — signet returned ~1684 sat/vByte, which
/// blew a swap fee up to ~0.003 BTC. Real fees there are ~1 sat/vByte, so cap low.
const TESTNET_SWAP_FEERATE_CAP_SAT_VB: u64 = 5;
/// Confirmation target for a rebalance tx — next block, so the fresh ladder
/// pieces become usable promptly (the rebalancer isn't fee-sensitive about a
/// background self-send, within the absolute cap the operator sets).
pub const REBALANCE_CONF_TARGET_BLOCKS: u32 = 1;

/// Clamp a raw `estimatesmartfee` result (sat/vByte) into the sane band a
/// rebalance tx uses: floored at 1 (so a missing estimate can't yield a
/// `minrelaytxfee`-rejected zero-fee tx) and capped per network (mainnet vs the
/// low test-net ceiling). Shared by the daemon executor and the `rebalance` CLI.
pub fn clamp_next_block_feerate(raw_estimate: u64, network: &BitcoinNetwork) -> u64 {
    let cap = match network {
        BitcoinNetwork::Mainnet => MAX_SWAP_FEERATE_SAT_VB,
        _ => TESTNET_SWAP_FEERATE_CAP_SAT_VB,
    };
    raw_estimate.clamp(MIN_SWAP_FEERATE_SAT_VB, cap)
}

/// Upper bound on reservation retries under contention. With outpoint
/// exclusion on each retry, the effective bound is `min(this, available_utxo_count)`
/// — the loop exits via the selector's Insufficient branch once exclusions
/// have whittled the candidate set to empty.
const RESERVE_RETRY_ATTEMPTS: u32 = 16;
/// Sell side: after the taker accepts, the maker holds its BTC reservation
/// this long awaiting the taker's consignment via `/consignment`. Building an
/// RGB consignment is a wallet operation, so the window is generous.
const CONSIGNMENT_TTL_MS: u64 = 600_000;

/// Maker-side state for a buy-side swap held between `accept_quote` and
/// `submit_signed_psbt`. The settlement state machine (#9) will formalize
/// this; for the mock a per-quote map keyed by `QuoteId` is enough.
#[derive(Clone)]
struct PendingBuySettlement {
    quote: Quote,
    reservation_id: ReservationId,
    /// Maker-built consignment handed to the taker at accept; needed again to
    /// finalize once the taker returns the signed PSBT.
    consignment: String,
}

/// Maker-side state for a sell-side swap. Created at `accept_quote` (stage
/// `AwaitingConsignment`, `psbt_built` is `None`) and advanced by
/// `deliver_consignment`, which fills `psbt_built` once the taker's
/// consignment has been validated and the swap PSBT built.
#[derive(Clone)]
struct PendingSellSettlement {
    quote: Quote,
    btc_reservation_id: ReservationId,
    btc_payout_addr: String,
    rgb_change_invoice: Option<String>,
    psbt_built: Option<SellPsbtBuilt>,
}

/// The `deliver_consignment` half of a sell-side settlement.
#[derive(Clone)]
struct SellPsbtBuilt {
    /// The taker's RGB-bearing outpoints from the validated consignment. The
    /// bait-and-switch check at `/sign` verifies the signed PSBT still spends
    /// every one of them.
    consigned_outpoints: Vec<Outpoint>,
    /// Witness txid committed at PSBT-build time; the signed PSBT must hash to it.
    expected_witness_txid: String,
    /// The taker's consignment, replayed into `finalize_after_taker_sign`.
    consignment: String,
}

#[derive(Clone)]
enum PendingSettlement {
    Buy(PendingBuySettlement),
    Sell(PendingSellSettlement),
}

#[derive(Clone)]
pub struct Maker {
    maker_id: MakerId,
    store: Arc<dyn InventoryStore>,
    selector: Arc<dyn CoinSelector>,
    /// Plain-BTC inventory the maker pays out from on the sell side. Empty
    /// unless seeded via `with_btc_inventory`; a buy-only maker never touches it.
    btc_store: Arc<dyn BtcInventoryStore>,
    /// Durable record of every consignment the maker produces, so it can be
    /// re-served on recovery. In-memory unless a durable store is injected via
    /// [`Maker::with_consignment_store`].
    consignment_store: Arc<dyn ConsignmentStore>,
    /// Durable record of every settled fill, recorded at broadcast. Feeds the
    /// `maker inventory` FILLED counter and the auto-mirror strategy. In-memory
    /// unless a durable store is injected via [`Maker::with_fills_store`].
    fills_store: Arc<dyn FillStore>,
    rgb_backend: Arc<dyn RgbBackend>,
    bitcoin_client: Arc<dyn BitcoinClient>,
    pending: Arc<RwLock<HashMap<QuoteId, PendingSettlement>>>,
    /// Standing-order prices. Empty unless seeded via [`Maker::with_price_policy`],
    /// in which case quotes use the configured per-asset price. Held in an
    /// `ArcSwap` so the daemon can hot-reload the order book without a restart
    /// (see [`Maker::reload_price_policy`]); reads are lock-free.
    price_policy: Arc<ArcSwap<PricePolicy>>,
    /// RGB ladder spec for settlement-piggyback: when set, buy-side settlements
    /// split the maker's RGB change into ladder rungs (riding the swap's fee).
    /// `None` ⇒ piggyback off (ordinary single change output). Set from the
    /// daemon's `[rebalance]` config when `enabled`.
    piggyback_rgb_ladder: Option<LadderSpec>,
    /// BTC ladder spec for sell-side piggyback: sells spend the maker's k0 BTC to
    /// pay takers, so the BTC change is split into this ladder to keep the pool
    /// laddered. `None` ⇒ off. Set alongside `piggyback_rgb_ladder`.
    piggyback_btc_ladder: Option<LadderSpec>,
}

impl Maker {
    /// Seed the inventory store from a per-UTXO chain view. All entries start
    /// `Available`; tests needing pre-existing Reserved / Spent state should
    /// build their own `InMemoryInventoryStore` and use `with_components`.
    pub fn new(
        maker_id: MakerId,
        utxos: Vec<RgbInventoryUtxo>,
        rgb_backend: Arc<dyn RgbBackend>,
        bitcoin_client: Arc<dyn BitcoinClient>,
    ) -> Self {
        let now = now_ms();
        let inv_utxos: Vec<InventoryUtxo> = utxos
            .into_iter()
            .map(|u| InventoryUtxo {
                outpoint: u.outpoint,
                asset_id: u.asset_id,
                amount: u.amount,
                btc_sats: u.btc_sats,
                status: InventoryStatus::Available,
                created_at_ms: now,
                updated_at_ms: now,
                pending_txid: None,
            })
            .collect();
        Self::with_components(
            maker_id,
            Arc::new(InMemoryInventoryStore::with_seed(inv_utxos)),
            Arc::new(GreedyExactFitSelector),
            rgb_backend,
            bitcoin_client,
        )
    }

    /// Full-control constructor: caller supplies its own inventory store and
    /// coin selector. Forward path for #9 (settlement state machine) once the
    /// store needs to be shared across components. The BTC inventory starts
    /// empty — seed it for sell-side quoting with [`Maker::with_btc_inventory`].
    pub fn with_components(
        maker_id: MakerId,
        store: Arc<dyn InventoryStore>,
        selector: Arc<dyn CoinSelector>,
        rgb_backend: Arc<dyn RgbBackend>,
        bitcoin_client: Arc<dyn BitcoinClient>,
    ) -> Self {
        Self {
            maker_id,
            store,
            selector,
            btc_store: Arc::new(InMemoryBtcInventoryStore::new()),
            consignment_store: Arc::new(InMemoryConsignmentStore::new()),
            fills_store: Arc::new(InMemoryFillStore::new()),
            rgb_backend,
            bitcoin_client,
            pending: Arc::new(RwLock::new(HashMap::new())),
            price_policy: Arc::new(ArcSwap::from_pointee(PricePolicy::default())),
            piggyback_rgb_ladder: None,
            piggyback_btc_ladder: None,
        }
    }

    /// Enable buy-side piggyback: buy swaps split the maker's RGB change into
    /// rungs of this ladder, riding the swap's existing fee. Off by default.
    pub fn with_piggyback_ladder(mut self, spec: LadderSpec) -> Self {
        self.piggyback_rgb_ladder = Some(spec);
        self
    }

    /// Enable sell-side piggyback: sell swaps split the maker's BTC change into
    /// rungs of this BTC ladder (keeping the k0 pool laddered). Off by default.
    pub fn with_piggyback_btc_ladder(mut self, spec: LadderSpec) -> Self {
        self.piggyback_btc_ladder = Some(spec);
        self
    }

    /// Compute the BTC-change rungs to piggyback onto a sell settlement. Empty
    /// unless a BTC ladder is configured and the maker's BTC change is large
    /// enough to carve ≥1 rung + remainder. `maker_btc_total` is the reserved BTC
    /// inputs' total; the on-tx change is `total - gross - anchor`.
    async fn compute_sell_btc_change_rungs(&self, maker_btc_total: u64, gross: u64) -> Vec<u64> {
        let Some(spec) = self.piggyback_btc_ladder.as_ref() else {
            return Vec::new();
        };
        let maker_change = maker_btc_total
            .saturating_sub(gross)
            .saturating_sub(REBALANCE_ANCHOR_SATS);
        if maker_change == 0 {
            return Vec::new();
        }
        let available: Vec<u64> = self
            .available_btc()
            .await
            .into_iter()
            .map(|(_, s)| s)
            .collect();
        plan_change_rungs(&available, maker_change, spec)
    }

    /// Compute the RGB-change rungs to piggyback onto a buy settlement. Empty
    /// (no piggyback) unless a ladder is configured and both gates pass: the
    /// change is large enough to carve ≥1 rung + remainder, and the maker's
    /// recycled seal-anchor sats can fund the rung anchors (546 each). Rungs are
    /// capped by the BTC available, smallest dropped first.
    async fn compute_buy_change_rungs(
        &self,
        asset: &AssetId,
        reserved: &[InventoryUtxo],
        amount: u64,
    ) -> Vec<u64> {
        let Some(spec) = self.piggyback_rgb_ladder.as_ref() else {
            return Vec::new();
        };
        let sum_in: u64 = reserved.iter().map(|u| u.amount).sum();
        let change = sum_in.saturating_sub(amount);
        if change == 0 {
            return Vec::new();
        }
        // Each rung output needs a 546-sat anchor from the recycled seal sats.
        let recycled: u64 = reserved.iter().map(|u| u.btc_sats).sum();
        let max_by_sats = (recycled / REBALANCE_ANCHOR_SATS) as usize;
        if max_by_sats == 0 {
            return Vec::new();
        }
        let available: Vec<u64> = self
            .store
            .list_available(asset)
            .await
            .iter()
            .map(|u| u.amount)
            .collect();
        let mut rungs = plan_change_rungs(&available, change, spec);
        rungs.truncate(max_by_sats); // rungs are largest-first; drop the smallest
        rungs
    }

    /// Seed the standing-order price policy (from the operator's saved orders).
    /// Without an order for an (asset, side) the maker declines quotes for it.
    pub fn with_price_policy(mut self, policy: PricePolicy) -> Self {
        self.price_policy = Arc::new(ArcSwap::from_pointee(policy));
        self
    }

    /// Hot-swap the standing-order prices (the daemon's order-reload loop calls
    /// this when the order book file changes). Lock-free; affects subsequent
    /// quotes immediately, across all cloned `Maker` handles (shared `ArcSwap`).
    pub fn reload_price_policy(&self, policy: PricePolicy) {
        self.price_policy.store(Arc::new(policy));
    }

    /// Seed the maker's plain-BTC inventory — the pool it pays sell-side takers
    /// from. Without this a maker quotes buy-side only; a sell `request_quote`
    /// finds no BTC to reserve and returns no quote.
    pub fn with_btc_inventory(mut self, utxos: Vec<BtcInventoryUtxo>) -> Self {
        self.btc_store = Arc::new(InMemoryBtcInventoryStore::with_seed(utxos));
        self
    }

    /// Inject a pre-built BTC inventory store (e.g. the durable SQLite one) in
    /// place of seeding an in-memory one via [`Maker::with_btc_inventory`].
    pub fn with_btc_store(mut self, store: Arc<dyn BtcInventoryStore>) -> Self {
        self.btc_store = store;
        self
    }

    /// Inject a durable consignment store (the SQLite one) in place of the
    /// default in-memory store, so produced consignments survive a restart.
    pub fn with_consignment_store(mut self, store: Arc<dyn ConsignmentStore>) -> Self {
        self.consignment_store = store;
        self
    }

    /// Inject a durable fill store (the SQLite one) so the FILLED counter +
    /// auto-mirror work-list survive a restart.
    pub fn with_fills_store(mut self, store: Arc<dyn FillStore>) -> Self {
        self.fills_store = store;
        self
    }

    /// Cumulative RGB units settled for `(asset_id, side)` since `since_ms`.
    /// Read API for the inventory FILLED display; returns 0 on store error.
    pub async fn filled_for(&self, asset_id: &str, side: &Side, since_ms: u64) -> u64 {
        self.fills_store
            .filled_for(asset_id, side, since_ms)
            .await
            .unwrap_or(0)
    }

    /// Fills the strategy loop hasn't mirrored yet (its work-list).
    pub async fn list_unmirrored_fills(&self) -> Vec<FillRecord> {
        self.fills_store.list_unmirrored().await.unwrap_or_default()
    }

    /// Mark a fill mirrored (after the strategy upserts the opposite order).
    pub async fn mark_fill_mirrored(&self, quote_id: &QuoteId) {
        if let Err(e) = self.fills_store.mark_mirrored(quote_id).await {
            eprintln!(
                "warning: failed to mark fill mirrored for quote {}: {e}",
                quote_id.0
            );
        }
    }

    /// Best-effort fill record at broadcast. Like [`Self::persist_consignment`],
    /// a failure is logged but never aborts the (already-broadcast) swap.
    async fn record_fill(&self, quote: &Quote, witness_txid: &str) {
        let record = FillRecord {
            quote_id: quote.quote_id.clone(),
            asset_id: quote.base_asset.id.clone(),
            side: quote.side.clone(),
            amount: quote.amount,
            price: quote.price,
            witness_txid: witness_txid.to_owned(),
            filled_at_ms: now_ms(),
            mirrored: false,
        };
        if let Err(e) = self.fills_store.record_fill(record).await {
            eprintln!(
                "warning: failed to record fill for quote {}: {e}",
                quote.quote_id.0
            );
        }
    }

    /// Best-effort persistence of a produced consignment. A failure is logged but
    /// never aborts the swap — the consignment is already returned to the taker;
    /// this store is the recovery safety net (re-served via the broker / CLI).
    async fn persist_consignment(
        &self,
        quote_id: &QuoteId,
        contract_id: &str,
        witness_txid: &str,
        consignment: &str,
    ) {
        let record = ConsignmentRecord {
            quote_id: quote_id.clone(),
            contract_id: contract_id.to_owned(),
            witness_txid: witness_txid.to_owned(),
            consignment: consignment.to_owned(),
            created_at_ms: now_ms(),
        };
        if let Err(e) = self.consignment_store.save_consignment(record).await {
            eprintln!(
                "warning: failed to persist consignment for quote {}: {e}",
                quote_id.0
            );
        }
    }

    /// Per-UTXO view across all assets. Returned in outpoint order so callers
    /// can index deterministically.
    pub async fn utxo_snapshot(&self) -> Vec<InventoryUtxo> {
        let mut utxos = self.store.list_all().await;
        utxos.sort_by(|a, b| a.outpoint.cmp(&b.outpoint));
        utxos
    }

    /// The maker's standing-order prices (per contract + side), for the broker's
    /// price feed. Mirrors the PricePolicy the maker quotes from, so the feed is
    /// consistent with actual quotes — including `available_size`, which folds in
    /// live inventory so a client doesn't size against depth the maker can't fill.
    pub async fn order_prices(&self) -> Vec<OrderPrice> {
        // Snapshot inventory once: available RGB per contract (Buy liquidity) and
        // the BTC pool the maker pays sells from (Sell liquidity).
        let rgb_utxos = self.store.list_all().await;
        let btc_available: u64 = self
            .btc_store
            .list_available()
            .await
            .iter()
            .map(|u| u.value_sats)
            .sum();

        self.price_policy
            .load()
            .entries()
            .iter()
            .map(|e| {
                let available_size = match e.side {
                    // Maker sends RGB → liquidity is its available RGB inventory
                    // for this contract.
                    Side::Buy => {
                        let have: u64 = rgb_utxos
                            .iter()
                            .filter(|u| {
                                u.asset_id.id == e.asset_id
                                    && matches!(u.status, InventoryStatus::Available)
                            })
                            .map(|u| u.amount)
                            .sum();
                        e.max_size.min(have)
                    }
                    // Maker pays BTC → liquidity is how many RGB units its BTC
                    // pool covers, net of the per-quote receive anchor.
                    Side::Sell if e.price_sats_per_unit > 0 => {
                        let spendable = btc_available.saturating_sub(rfq_rgb::SEAL_ANCHOR_SATS);
                        e.max_size.min(spendable / e.price_sats_per_unit)
                    }
                    Side::Sell => 0,
                };
                OrderPrice {
                    contract_id: e.asset_id.clone(),
                    side: e.side.clone(),
                    price_sats_per_unit: e.price_sats_per_unit,
                    max_size: e.max_size,
                    available_size,
                }
            })
            .collect()
    }

    /// The distinct RGB contracts this maker serves, each with display metadata
    /// (ticker + precision) read from the contract. Advertised to the broker so
    /// it can build an asset directory. Ticker/precision fall back to empty/0 if
    /// the contract spec can't be read.
    pub async fn served_assets(&self) -> Vec<AssetInfo> {
        let mut seen: Vec<AssetId> = Vec::new();
        let mut infos: Vec<AssetInfo> = Vec::new();
        for u in self.utxo_snapshot().await {
            if seen.contains(&u.asset_id) {
                continue;
            }
            seen.push(u.asset_id.clone());
            let (ticker, precision) = self
                .rgb_backend
                .asset_spec(&u.asset_id)
                .await
                .unwrap_or_else(|_| (String::new(), 0));
            infos.push(AssetInfo {
                id: u.asset_id,
                ticker,
                precision,
            });
        }
        infos
    }

    pub async fn inventory_summary(&self) -> InventorySnapshot {
        let ext = self.extended_inventory_summary().await;
        (&ext).into()
    }

    /// Forward-looking metric surface used by 14e's rebalance loop. The legacy
    /// `inventory_summary` keeps wire-format stability for the HTTP layer.
    pub async fn extended_inventory_summary(&self) -> ExtendedInventorySnapshot {
        self.store.release_expired_reservations(now_ms()).await;
        let utxos = self.store.list_all().await;
        ExtendedInventorySnapshot::from_utxos(utxos.iter())
    }

    /// Per-asset inventory snapshot — the multi-asset view, for a maker trading
    /// more than one contract. `inventory_summary` aggregates ALL assets; this
    /// scopes to one (so amounts render in that contract's ticker/precision).
    pub async fn inventory_summary_for(&self, asset: &AssetId) -> InventorySnapshot {
        self.store.release_expired_reservations(now_ms()).await;
        (&self.store.extended_snapshot(asset).await).into()
    }

    /// Bulk-ingest BTC UTXOs discovered out-of-band (e.g. by the chain
    /// observer's periodic re-list of wallet-derived BTC inventory) into
    /// the BTC store. Existing outpoints are silently skipped — the store's
    /// `ingest_change_utxo` returns `UtxoNotAvailable` on duplicates, which
    /// we treat as a no-op (the chain observer re-lists everything each
    /// tick; most entries are already known). Returns the count actually
    /// inserted. Issue #27.
    pub async fn ingest_btc_change_utxos(&self, utxos: Vec<BtcInventoryUtxo>) -> usize {
        let mut added = 0;
        for u in utxos {
            if self.btc_store.ingest_change_utxo(u).await.is_ok() {
                added += 1;
            }
        }
        added
    }

    /// Same shape as [`Self::ingest_btc_change_utxos`] but for RGB. Required
    /// for consecutive maker-side swaps: each swap consumes the maker's
    /// RGB input and produces an RGB-change output at a new outpoint;
    /// without this re-ingestion, `request_quote`'s coin selector sees
    /// only the original (now-spent) entry in the inventory store and
    /// returns `None` on the second swap. The chain observer calls this
    /// after `sync_wallet` so the new outpoint is in the bp-wallet cache.
    pub async fn ingest_rgb_change_utxos(&self, utxos: Vec<RgbInventoryUtxo>) -> usize {
        let now = now_ms();
        let mut added = 0;
        for raw in utxos {
            let inv = InventoryUtxo {
                outpoint: raw.outpoint,
                asset_id: raw.asset_id,
                amount: raw.amount,
                btc_sats: raw.btc_sats,
                status: InventoryStatus::Available,
                created_at_ms: now,
                updated_at_ms: now,
                pending_txid: None,
            };
            if self.store.ingest_change_utxo(inv).await.is_ok() {
                added += 1;
            }
        }
        added
    }

    /// For every reservation currently in `PendingBitcoinConfirm` — in
    /// either the BTC store or the RGB store — probe the chain via
    /// `BitcoinClient::get_outpoint` to see if the witness tx is on-chain;
    /// if so, transition the reservation to `Spent`. Returns the count of
    /// UTXOs that moved. Issue #27 (BTC side) + RGB-side follow-up.
    ///
    /// Both sides are swept in the same pass because a swap commits both
    /// the maker's RGB inputs and (for sells) the BTC payout under the
    /// same witness tx; probing once per distinct witness_txid avoids
    /// duplicate electrum round-trips.
    ///
    /// Confirmation probe: we ask for `(witness_txid, 0)`. Under tapret vout 0
    /// is the maker's P2TR commitment-host output, so a confirmed tx replies
    /// `Ok`/`OutpointNotFound`; a still-missing tx replies with `Backend`
    /// (typically "transaction not found"). Any "tx exists in some form"
    /// response counts as confirmed; everything else is "not yet."
    pub async fn sweep_confirmations(&self) -> usize {
        use std::collections::{HashMap, HashSet};

        let mut btc_pending: HashMap<ReservationId, String> = HashMap::new();
        for u in self.btc_store.list_all().await {
            if let BtcInventoryStatus::PendingBitcoinConfirm {
                reservation_id,
                witness_txid,
            } = u.status
            {
                btc_pending.insert(reservation_id, witness_txid);
            }
        }

        let mut rgb_pending: HashMap<ReservationId, String> = HashMap::new();
        for u in self.store.list_all().await {
            if let InventoryStatus::PendingBitcoinConfirm {
                reservation_id,
                witness_txid,
            } = u.status
            {
                rgb_pending.insert(reservation_id, witness_txid);
            }
        }

        let mut confirmed: HashMap<String, bool> = HashMap::new();
        let txids: HashSet<&str> = btc_pending
            .values()
            .chain(rgb_pending.values())
            .map(String::as_str)
            .collect();
        for txid in txids {
            confirmed.insert(
                txid.to_owned(),
                tx_confirmed(&*self.bitcoin_client, txid).await,
            );
        }

        let mut spent = 0;
        for (reservation_id, witness_txid) in btc_pending {
            if !confirmed.get(&witness_txid).copied().unwrap_or(false) {
                continue;
            }
            if let Ok(n) = self
                .btc_store
                .mark_spent(&reservation_id, witness_txid, now_ms())
                .await
            {
                spent += n;
            }
        }
        for (reservation_id, witness_txid) in rgb_pending {
            if !confirmed.get(&witness_txid).copied().unwrap_or(false) {
                continue;
            }
            if let Ok(n) = self
                .store
                .mark_spent(&reservation_id, witness_txid, now_ms())
                .await
            {
                spent += n;
            }
        }
        spent
    }

    pub async fn release_expired_reservations(&self) -> usize {
        let now = now_ms();
        // Both pools age out here, so the maker-node cleanup loop's single call
        // covers buy-side RGB reservations and sell-side BTC reservations alike.
        let released = self.store.release_expired_reservations(now).await
            + self.btc_store.release_expired_reservations(now).await;
        // Prune settlement state whose reservation no longer exists — either
        // expired above, or already transitioned out of `Reserved` by a
        // completed settlement. `find_reservation_for_quote` only matches
        // `Reserved`; a settled quote reads as "gone", which is fine — the
        // settlement path already removed its own entry. Buy and sell track
        // their reservation in different stores.
        let entries: Vec<(QuoteId, bool)> = self
            .pending
            .read()
            .await
            .iter()
            .map(|(quote_id, p)| (quote_id.clone(), matches!(p, PendingSettlement::Buy(_))))
            .collect();
        let mut stale = Vec::new();
        for (quote_id, is_buy) in entries {
            let live = if is_buy {
                self.store
                    .find_reservation_for_quote(&quote_id)
                    .await
                    .is_some()
            } else {
                self.btc_store
                    .find_reservation_for_quote(&quote_id)
                    .await
                    .is_some()
            };
            if !live {
                stale.push(quote_id);
            }
        }
        if !stale.is_empty() {
            let mut pending = self.pending.write().await;
            for quote_id in stale {
                pending.remove(&quote_id);
            }
        }
        released
    }

    /// Periodic rebalance planner. Reads `extended_inventory_summary()` and
    /// returns a `RebalancePlan` describing trigger reasons plus (in the
    /// follow-up issue) the merge/split actions to fold into the next
    /// outgoing settlement tx. In 14e the action lists stay empty — only the
    /// trigger detection ships. See `docs/rebalancing-strategy.md`.
    pub async fn rebalance(&self, policy: &RebalancePolicy) -> RebalancePlan {
        let ext = self.extended_inventory_summary().await;
        let mut plan = RebalancePlan::default();

        // High fragmentation is only meaningful when there's something to
        // rebalance — guard against div-by-zero artifacts on empty inventory.
        if ext.available_amount > 0 && ext.fragmentation_score >= policy.fragmentation_threshold {
            plan.triggers.push(RebalanceTrigger::HighFragmentation {
                score: ext.fragmentation_score,
                threshold: policy.fragmentation_threshold,
            });
        }
        if ext.available_utxos > policy.max_utxo_count {
            plan.triggers.push(RebalanceTrigger::TooManyUtxos {
                count: ext.available_utxos,
                max: policy.max_utxo_count,
            });
        }
        if ext.available_utxos < policy.min_utxo_count && ext.total_amount > 0 {
            plan.triggers.push(RebalanceTrigger::TooFewUtxos {
                count: ext.available_utxos,
                min: policy.min_utxo_count,
            });
        }

        plan
    }

    // --- rebalance executor support ---------------------------------------
    //
    // Reservations made for an internal rebalance split reuse the quote-bound
    // reserve path under a sentinel rfq id (no store-schema change). The maker's
    // quoting path never collides with it — real ids come from broker requests.

    /// `(outpoint, rgb_amount, btc_sats)` for every Available allocation of
    /// `asset` — `plan_ladder` consumes the first two, `AssetSplit` the sats.
    pub async fn available_rgb_for(&self, asset: &AssetId) -> Vec<(Outpoint, u64, u64)> {
        self.store.release_expired_reservations(now_ms()).await;
        self.store
            .list_available(asset)
            .await
            .into_iter()
            .map(|u| (u.outpoint, u.amount, u.btc_sats))
            .collect()
    }

    /// `(outpoint, sats)` for every Available BTC UTXO.
    pub async fn available_btc(&self) -> Vec<(Outpoint, u64)> {
        self.btc_store.release_expired_reservations(now_ms()).await;
        self.btc_store
            .list_available()
            .await
            .into_iter()
            .map(|u| (u.outpoint, u.value_sats))
            .collect()
    }

    /// Atomically claim `source` (an RGB allocation) for an internal split so the
    /// quoting path won't select it mid-rebalance. Long TTL — the source must stay
    /// reserved until its split tx confirms (then `mark_*_rebalance_pending` +
    /// `sweep_confirmations` retire it).
    pub async fn reserve_rgb_for_rebalance(
        &self,
        source: Outpoint,
    ) -> Result<ReservationId, InventoryError> {
        let now = now_ms();
        self.store
            .reserve_utxos(
                &RfqId(REBALANCE_RFQ_ID.to_owned()),
                &QuoteId(format!("{REBALANCE_RFQ_ID}:{}", Uuid::new_v4())),
                &[source],
                now + BROADCAST_CONFIRM_TTL_MS,
                now,
            )
            .await
    }

    /// BTC analogue of [`Self::reserve_rgb_for_rebalance`].
    pub async fn reserve_btc_for_rebalance(
        &self,
        source: Outpoint,
    ) -> Result<ReservationId, rfq_types::BtcInventoryError> {
        let now = now_ms();
        self.btc_store
            .reserve(
                &RfqId(REBALANCE_RFQ_ID.to_owned()),
                &QuoteId(format!("{REBALANCE_RFQ_ID}:{}", Uuid::new_v4())),
                &[source],
                now + BROADCAST_CONFIRM_TTL_MS,
                now,
            )
            .await
    }

    /// Move a reserved RGB source to `PendingBitcoinConfirm` after the split tx
    /// broadcasts, so `sweep_confirmations` retires it once the tx confirms.
    pub async fn mark_rgb_rebalance_pending(&self, reservation_id: &ReservationId, txid: String) {
        let _ = self
            .store
            .mark_pending_bitcoin_confirm(reservation_id, txid, now_ms())
            .await;
    }

    /// BTC analogue of [`Self::mark_rgb_rebalance_pending`].
    pub async fn mark_btc_rebalance_pending(&self, reservation_id: &ReservationId, txid: String) {
        let _ = self
            .btc_store
            .mark_pending_bitcoin_confirm(reservation_id, txid, now_ms())
            .await;
    }

    /// Release rebalance reservations (RGB then BTC) back to Available — called
    /// when the split fails to build or broadcast, before anything was marked
    /// pending.
    pub async fn release_rebalance(&self, rgb: &[ReservationId], btc: Option<&ReservationId>) {
        let now = now_ms();
        for r in rgb {
            let _ = self.store.release_reservation(r, now).await;
        }
        if let Some(r) = btc {
            let _ = self.btc_store.release_reservation(r, now).await;
        }
    }

    /// True while a previously-launched rebalance split is still settling — its
    /// RGB source(s) are no longer Available but not yet `Spent`. Lets the loop
    /// skip planning so it doesn't pile a second split on top of an unconfirmed
    /// one. `false` once every source reads `Spent` (or is gone).
    pub async fn rgb_sources_settled(&self, sources: &[Outpoint]) -> bool {
        for op in sources {
            match self.store.get(op).await {
                Some(u) => {
                    if !matches!(u.status, InventoryStatus::Spent { .. }) {
                        return false;
                    }
                }
                None => continue,
            }
        }
        true
    }

    /// Next-block feerate (sat/vByte) for a rebalance tx, clamped to the same
    /// sane band as swaps so a missing estimate can't zero-fee the tx (it would
    /// be rejected under `minrelaytxfee`) and a wild one can't overpay. The
    /// executor multiplies this by the tx's estimated vsize, then applies the
    /// operator's absolute fee cap.
    pub async fn next_block_feerate(&self, network: &BitcoinNetwork) -> u64 {
        let raw = self
            .bitcoin_client
            .estimate_feerate(REBALANCE_CONF_TARGET_BLOCKS)
            .await
            .unwrap_or(0);
        clamp_next_block_feerate(raw, network)
    }

    /// Broadcast a finished rebalance tx via the maker's bitcoin client.
    pub async fn broadcast_rebalance(&self, raw_tx: &[u8]) -> Result<String, RouterError> {
        self.bitcoin_client
            .broadcast(raw_tx)
            .await
            .map_err(|e| RouterError::Maker(format!("rebalance broadcast: {e}")))
    }

    /// Sell-side `request_quote`: the maker reserves BTC to pay the taker and
    /// mints the RGB invoice the taker will consign to. Returns `Ok(None)`
    /// when the BTC pool can't cover the gross payout.
    async fn request_quote_sell(
        &self,
        request: QuoteRequest,
        quote_id: QuoteId,
        expires_at_ms: u64,
    ) -> Result<Option<Quote>, RouterError> {
        let amount = request.amount;
        // Same pricing rule as the buy side; `price` is the gross BTC the
        // maker pays out, before the network fee the taker covers. A standing
        // order may decline (amount over its size) before we reserve BTC.
        let gross_btc_sats =
            match self
                .price_policy
                .load()
                .unit_price(&request.base_asset, &request.side, amount)
            {
                PriceLookup::Price(p) => p.saturating_mul(amount),
                // Surface the decline (else a sell returns no quote and "looks
                // broken"): either no standing order, or the amount is over its size.
                PriceLookup::NoOrder => {
                    eprintln!(
                        "declining sell quote: no standing sell order for asset {} — create one \
                         with `colorex maker order create --side sell`",
                        request.base_asset.id,
                    );
                    return Ok(None);
                }
                PriceLookup::Decline => {
                    eprintln!(
                        "declining sell quote: amount {amount} over the sell order's max size \
                         for asset {} — raise the order size or sell less",
                        request.base_asset.id,
                    );
                    return Ok(None);
                }
            };

        let available = self.btc_store.list_available().await;
        // The maker funds its own witness-vout RGB-receive anchor on top of the
        // gross payout, so reserve gross + SEAL_ANCHOR_SATS. The network fee is the
        // taker's — netted from its payout, not drawn from the maker's inventory.
        let required = gross_btc_sats.saturating_add(rfq_rgb::SEAL_ANCHOR_SATS);
        let selection = match GreedyLargestFirstSelector.select(required, &available) {
            Ok(s) => s,
            // Not enough BTC on hand — decline the quote rather than error, but log
            // why (else a sell silently returns no quote and looks broken). The
            // maker pays sell-side takers from its own keychain-0 BTC inventory.
            Err(_) => {
                let have: u64 = available.iter().map(|u| u.value_sats).sum();
                eprintln!(
                    "declining sell quote: BTC inventory {have} sats < needed {required} \
                     (gross {gross_btc_sats} + anchor {}) — fund the maker's keychain-0 wallet",
                    rfq_rgb::SEAL_ANCHOR_SATS,
                );
                return Ok(None);
            }
        };
        let reservation_id = match self
            .btc_store
            .reserve(
                &request.rfq_id,
                &quote_id,
                &selection.chosen,
                expires_at_ms,
                now_ms(),
            )
            .await
        {
            Ok(rid) => rid,
            // Lost a race for one of the chosen UTXOs — treat as no quote.
            Err(_) => return Ok(None),
        };

        // No maker invoice (provenance model): the taker exports a provenance
        // consignment for its own outpoints and names them on the wire; the maker
        // mints nothing and needs no spare anchor. The `reservation_id` holds the
        // maker's BTC payout until /consignment. See
        // docs/provenance-consignment-proposal.md.
        let _ = reservation_id;

        let estimated_fee_sats = self.estimate_swap_fee(&request.base_asset.network).await;
        Ok(Some(Quote {
            quote_id,
            rfq_id: request.rfq_id,
            maker_id: self.maker_id.clone(),
            base_asset: request.base_asset,
            quote_asset: request.quote_asset,
            side: request.side,
            amount,
            price: gross_btc_sats,
            expires_at_ms,
            estimated_fee_sats,
            fee_slippage_bps: 2000,
            maker_rgb_invoice: None,
        }))
    }

    /// Quote-time fee estimate: feerate × the rough swap-tx vbyte footprint. A
    /// failed estimate falls back to 0, which disables the slippage check at
    /// settlement (no baseline to compare against) rather than rejecting the
    /// quote outright.
    async fn estimate_swap_fee(&self, network: &BitcoinNetwork) -> u64 {
        let cap = match network {
            BitcoinNetwork::Mainnet => MAX_SWAP_FEERATE_SAT_VB,
            _ => TESTNET_SWAP_FEERATE_CAP_SAT_VB,
        };
        let feerate = self
            .bitcoin_client
            .estimate_feerate(3)
            .await
            .unwrap_or(0)
            .clamp(MIN_SWAP_FEERATE_SAT_VB, cap);
        feerate.saturating_mul(ESTIMATED_SWAP_VBYTES)
    }

    /// Sell-side `accept_quote`: validate the payout address, re-confirm the
    /// BTC reservation made at quote time, stretch it to the consignment
    /// window, and park a `PendingSellSettlement` in `AwaitingConsignment`.
    async fn accept_quote_sell(
        &self,
        quote: Quote,
        btc_payout_addr: String,
        rgb_change_invoice: Option<String>,
    ) -> Result<SettlementIntent, RouterError> {
        self.btc_store.release_expired_reservations(now_ms()).await;

        // Mock address check — real validation parses the bech32 / base58 form.
        if btc_payout_addr.trim().is_empty() {
            return Err(RouterError::Maker(
                "sell leg is missing a BTC payout address".to_owned(),
            ));
        }

        let btc_reservation_id = self
            .btc_store
            .find_reservation_for_quote(&quote.quote_id)
            .await
            .ok_or_else(|| {
                RouterError::Maker("quote BTC reservation not found or expired".to_owned())
            })?;

        // The quote-stage TTL is short; stretch it to the consignment window
        // so the cleanup loop doesn't reclaim the BTC mid-flight.
        let expires_at_ms = now_ms() + CONSIGNMENT_TTL_MS;
        self.btc_store
            .extend_reservation(&btc_reservation_id, expires_at_ms, now_ms())
            .await
            .map_err(|e| RouterError::Maker(e.to_string()))?;

        let quote_id = quote.quote_id.clone();
        self.pending.write().await.insert(
            quote_id.clone(),
            PendingSettlement::Sell(PendingSellSettlement {
                quote,
                btc_reservation_id,
                btc_payout_addr,
                rgb_change_invoice,
                psbt_built: None,
            }),
        );

        Ok(SettlementIntent {
            quote_id,
            maker_id: self.maker_id.clone(),
            status: SettlementStatus::AwaitingConsignment,
            transfer: None,
            expires_at_ms,
            witness_txid: None,
            final_consignment: None,
        })
    }

    /// Sell-side `/consignment`: validate the taker's consignment, resolve the
    /// prevouts it names, re-check fee slippage, build + maker-sign the swap
    /// PSBT, and advance the settlement to `AwaitingTakerSignature`.
    async fn deliver_consignment_sell(
        &self,
        quote_id: QuoteId,
        consignment_base64: String,
        consigned_outpoints: Vec<Outpoint>,
    ) -> Result<SettlementIntent, RouterError> {
        self.btc_store.release_expired_reservations(now_ms()).await;

        let pending = match self.pending.read().await.get(&quote_id).cloned() {
            Some(PendingSettlement::Sell(p)) => p,
            // A buy-side entry, or no accepted sell quote at all.
            _ => {
                return Err(RouterError::Maker(
                    "no accepted sell-side settlement for quote".to_owned(),
                ))
            }
        };

        // A vanished reservation means the consignment window lapsed.
        let btc_reservation_id = match self.btc_store.find_reservation_for_quote(&quote_id).await {
            Some(rid) => rid,
            None => {
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::Maker(
                    "consignment window lapsed before delivery".to_owned(),
                ));
            }
        };

        let quote = &pending.quote;
        // Provenance model (no maker invoice): the contract is the quote's base
        // asset; the taker names the outpoints it's selling on the wire and the
        // consignment proves their provenance. See
        // docs/provenance-consignment-proposal.md.
        let contract_id = ContractId::from_str(&quote.base_asset.id).map_err(|e| {
            RouterError::Maker(format!("quote carries an invalid contract id: {e}"))
        })?;

        // Validate the consignment + confirm the named outpoints carry the RGB.
        // (Chain existence / unspent + prevout come from `get_outpoint` just below;
        // the taker's signature authorizes the spend at /sign.)
        let info = match self
            .rgb_backend
            .validate_incoming_consignment(&consignment_base64, contract_id, &consigned_outpoints)
            .await
        {
            Ok(info) => info,
            Err(e) => {
                let _ = self
                    .btc_store
                    .mark_broadcast_failed(&btc_reservation_id, now_ms())
                    .await;
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::ConsignmentRejected(e.to_string()));
            }
        };

        // Resolve the prevout behind every consigned RGB outpoint — needed to
        // pin them as PSBT inputs. An outpoint the chain doesn't know (or a
        // non-segwit one) makes the consignment unusable.
        let mut taker_rgb_prevouts = Vec::with_capacity(info.outpoints.len());
        for outpoint in &info.outpoints {
            match self.bitcoin_client.get_outpoint(outpoint).await {
                Ok(txout) => taker_rgb_prevouts.push((outpoint.clone(), txout)),
                Err(e) => {
                    let _ = self
                        .btc_store
                        .mark_broadcast_failed(&btc_reservation_id, now_ms())
                        .await;
                    self.pending.write().await.remove(&quote_id);
                    return Err(RouterError::ConsignmentRejected(format!(
                        "consignment names an unusable outpoint {outpoint}: {e}"
                    )));
                }
            }
        }

        // The maker's own BTC inputs are the UTXOs reserved under this quote;
        // their value + script live in the BTC store, so no chain round trip.
        let maker_btc_inputs: Vec<(Outpoint, TxOut)> = self
            .btc_store
            .list_all()
            .await
            .into_iter()
            .filter(|u| {
                matches!(&u.status,
                    BtcInventoryStatus::Reserved { reservation_id: rid, .. }
                        if rid == &btc_reservation_id)
            })
            .map(|u| {
                (
                    u.outpoint,
                    TxOut {
                        value_sats: u.value_sats,
                        script_pubkey: u.script_pubkey,
                    },
                )
            })
            .collect();

        // Re-estimate the fee and abort if it blew the quote's slippage cap.
        let actual_fee = self.estimate_swap_fee(&quote.base_asset.network).await;
        if quote.estimated_fee_sats > 0 {
            let cap = quote
                .estimated_fee_sats
                .saturating_mul(10_000 + u64::from(quote.fee_slippage_bps))
                / 10_000;
            if actual_fee > cap {
                let _ = self
                    .btc_store
                    .mark_broadcast_failed(&btc_reservation_id, now_ms())
                    .await;
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::FeeSlippageExceeded {
                    estimated: quote.estimated_fee_sats,
                    actual: actual_fee,
                });
            }
        }

        // Build the swap PSBT (maker BTC inputs signed in the mock string).
        // `deliver_amount` is the RGB amount the taker is selling (the quote
        // amount); falls back to the consigned total if the quote omitted it.
        let deliver_amount = if quote.amount > 0 {
            quote.amount
        } else {
            info.total_amount
        };
        // Sell-side piggyback: split the maker's BTC change into a k0 ladder.
        let maker_btc_total: u64 = maker_btc_inputs.iter().map(|(_, t)| t.value_sats).sum();
        let btc_change_rungs = self
            .compute_sell_btc_change_rungs(maker_btc_total, quote.price)
            .await;
        let transfer = match self
            .rgb_backend
            .create_swap_psbt_sell(
                &info,
                &taker_rgb_prevouts,
                &maker_btc_inputs,
                contract_id,
                deliver_amount,
                &pending.btc_payout_addr,
                pending.rgb_change_invoice.as_deref(),
                quote.price,
                actual_fee,
                &btc_change_rungs,
            )
            .await
        {
            Ok(t) => t,
            Err(e) => {
                let _ = self
                    .btc_store
                    .mark_broadcast_failed(&btc_reservation_id, now_ms())
                    .await;
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::Maker(e.to_string()));
            }
        };

        let expected_witness_txid = transfer.expected_witness_txid.clone().ok_or_else(|| {
            RouterError::Maker("sell PSBT carries no committed witness txid".to_owned())
        })?;

        // Stretch the reservation to the taker-signature window.
        let expires_at_ms = now_ms() + TAKER_SIGNATURE_TTL_MS;
        self.btc_store
            .extend_reservation(&btc_reservation_id, expires_at_ms, now_ms())
            .await
            .map_err(|e| RouterError::Maker(e.to_string()))?;

        // Record what `/sign` will re-check the taker's PSBT against, plus the
        // consignment to echo back as `final_consignment`. When the taker
        // over-consigned, `create_swap_psbt_sell` returns the maker-emitted
        // change consignment (addressed to the taker's change seal, anchored to
        // the real swap witness) — that's what lets the taker record its RGB
        // change. Falls back to the taker's own consignment when there's no
        // change (and for the mock backend, which emits none).
        let final_consignment = transfer.consignment.clone().unwrap_or(consignment_base64);
        if let Some(PendingSettlement::Sell(p)) = self.pending.write().await.get_mut(&quote_id) {
            p.psbt_built = Some(SellPsbtBuilt {
                consigned_outpoints: info.outpoints,
                expected_witness_txid,
                consignment: final_consignment,
            });
        }

        Ok(SettlementIntent {
            quote_id,
            maker_id: self.maker_id.clone(),
            status: SettlementStatus::AwaitingTakerSignature,
            transfer: Some(transfer),
            expires_at_ms,
            witness_txid: None,
            final_consignment: None,
        })
    }

    /// Sell-side `/sign`: guard against input swaps and txid drift, then
    /// finalize, broadcast, and move the maker's BTC to `PendingBitcoinConfirm`.
    async fn submit_signed_psbt_sell(
        &self,
        quote_id: QuoteId,
        pending: PendingSellSettlement,
        signed_psbt_base64: String,
    ) -> Result<SettlementIntent, RouterError> {
        let built = pending.psbt_built.as_ref().ok_or_else(|| {
            RouterError::Maker("consignment not yet delivered for this quote".to_owned())
        })?;
        let btc_reservation_id = pending.btc_reservation_id.clone();

        // A vanished reservation means the taker-signature window lapsed.
        if self
            .btc_store
            .find_reservation_for_quote(&quote_id)
            .await
            .is_none()
        {
            self.pending.write().await.remove(&quote_id);
            return Err(RouterError::Maker(
                "settlement expired before signature was submitted".to_owned(),
            ));
        }

        // Bait-and-switch + txid guard: the signed PSBT must still spend every
        // outpoint the validated consignment named and still commit to the
        // witness txid fixed at build time. Delegated to the backend so the
        // check works on real (base64) PSBTs, not just the mock's plaintext.
        if let Err(e) = self
            .rgb_backend
            .verify_signed_swap_psbt(
                &signed_psbt_base64,
                &built.consigned_outpoints,
                &built.expected_witness_txid,
            )
            .await
        {
            let _ = self
                .btc_store
                .mark_broadcast_failed(&btc_reservation_id, now_ms())
                .await;
            self.pending.write().await.remove(&quote_id);
            return Err(RouterError::PsbtInvalid(e.to_string()));
        }

        let finalized = match self
            .rgb_backend
            .finalize_after_taker_sign(&signed_psbt_base64, &built.consignment)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                let _ = self
                    .btc_store
                    .mark_broadcast_failed(&btc_reservation_id, now_ms())
                    .await;
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::PsbtInvalid(e.to_string()));
            }
        };

        // Broadcast. A failure here releases the BTC — the swap tx never hit
        // the network, so the maker's UTXOs are still spendable.
        if let Err(e) = self.bitcoin_client.broadcast(&finalized.raw_tx).await {
            let _ = self
                .btc_store
                .mark_broadcast_failed(&btc_reservation_id, now_ms())
                .await;
            self.pending.write().await.remove(&quote_id);
            return Err(RouterError::Maker(format!("broadcast failed: {e}")));
        }

        self.btc_store
            .mark_pending_bitcoin_confirm(
                &btc_reservation_id,
                finalized.witness_txid.clone(),
                now_ms(),
            )
            .await
            .map_err(|e| RouterError::Maker(e.to_string()))?;

        // Persist the maker-emitted consignment (sell: the RGB change transfer to
        // the taker's change seal) for recovery.
        self.persist_consignment(
            &quote_id,
            &pending.quote.base_asset.id,
            &finalized.witness_txid,
            &finalized.final_consignment_base64,
        )
        .await;

        // Record the fill (FILLED counter + auto-mirror work-list).
        self.record_fill(&pending.quote, &finalized.witness_txid)
            .await;

        self.pending.write().await.remove(&quote_id);

        Ok(SettlementIntent {
            quote_id,
            maker_id: self.maker_id.clone(),
            status: SettlementStatus::PendingBitcoinConfirm,
            transfer: None,
            expires_at_ms: now_ms() + BROADCAST_CONFIRM_TTL_MS,
            witness_txid: Some(finalized.witness_txid),
            final_consignment: Some(finalized.final_consignment_base64),
        })
    }
}

#[async_trait]
impl MakerConnector for Maker {
    fn maker_id(&self) -> MakerId {
        self.maker_id.clone()
    }

    async fn request_prices(&self) -> Result<Vec<OrderPrice>, RouterError> {
        Ok(self.order_prices().await)
    }

    async fn request_quote(&self, request: QuoteRequest) -> Result<Option<Quote>, RouterError> {
        let now = now_ms();
        self.store.release_expired_reservations(now).await;
        self.btc_store.release_expired_reservations(now).await;

        let quote_id = QuoteId(Uuid::new_v4().to_string());
        let expires_at_ms = now + QUOTE_TTL_MS;

        // Sell side reserves BTC, not RGB — the maker is the one paying out.
        if matches!(request.side, Side::Sell) {
            return self
                .request_quote_sell(request, quote_id, expires_at_ms)
                .await;
        }

        // Resolve the unit price up front: a standing order may decline the
        // quote (amount over its size) before we touch inventory.
        let buy_unit_price = match self.price_policy.load().unit_price(
            &request.base_asset,
            &request.side,
            request.amount,
        ) {
            PriceLookup::Price(p) => p,
            PriceLookup::NoOrder => {
                eprintln!(
                    "declining buy quote: no standing buy order for asset {} — create one \
                         with `colorex maker order create --side buy`",
                    request.base_asset.id,
                );
                return Ok(None);
            }
            PriceLookup::Decline => {
                eprintln!(
                    "declining buy quote: amount {} over the buy order's max size for asset {} \
                         — raise the order size or buy less",
                    request.amount, request.base_asset.id,
                );
                return Ok(None);
            }
        };

        // Exclusion-based retry: when a reservation fails because another
        // caller grabbed an outpoint between our list_available read and our
        // reserve_utxos write, we exclude the contested outpoint and re-select.
        // This is what gives a deterministic selector (GreedyExactFitSelector)
        // healthy concurrency — without it, every losing task re-picks the
        // same UTXO and only one ever makes progress per round.
        let mut excluded: HashSet<Outpoint> = HashSet::new();
        let mut attempts: u32 = 0;
        let selection = loop {
            attempts += 1;
            let available: Vec<InventoryUtxo> = self
                .store
                .list_available(&request.base_asset)
                .await
                .into_iter()
                .filter(|u| !excluded.contains(&u.outpoint))
                .collect();
            let selection =
                match self
                    .selector
                    .select(&request.base_asset, request.amount, &available)
                {
                    Ok(s) => s,
                    Err(CoinSelectionError::Insufficient { .. }) => return Ok(None),
                    Err(e) => return Err(RouterError::Maker(e.to_string())),
                };
            match self
                .store
                .reserve_utxos(
                    &request.rfq_id,
                    &quote_id,
                    &selection.chosen,
                    expires_at_ms,
                    now_ms(),
                )
                .await
            {
                Ok(_) => break selection,
                Err(InventoryError::UtxoNotAvailable { .. })
                | Err(InventoryError::UtxoNotFound(_))
                    if attempts < RESERVE_RETRY_ATTEMPTS =>
                {
                    excluded.extend(selection.chosen);
                    continue;
                }
                Err(InventoryError::UtxoNotAvailable { .. })
                | Err(InventoryError::UtxoNotFound(_)) => return Ok(None),
                Err(e) => return Err(RouterError::Maker(e.to_string())),
            }
        };

        let estimated_fee_sats = self.estimate_swap_fee(&request.base_asset.network).await;

        Ok(Some(Quote {
            quote_id,
            rfq_id: request.rfq_id,
            maker_id: self.maker_id.clone(),
            base_asset: request.base_asset,
            quote_asset: request.quote_asset,
            side: request.side,
            amount: selection.requested,
            price: buy_unit_price.saturating_mul(selection.requested),
            expires_at_ms,
            estimated_fee_sats,
            // 20% slippage cap — the v0 default per docs/swap-flows.md.
            fee_slippage_bps: 2000,
            // Buy-side quotes never carry a maker RGB invoice — the taker
            // supplies its own at `/accept`. Sell side sets it (see
            // `request_quote_sell`).
            maker_rgb_invoice: None,
        }))
    }

    async fn accept_quote(
        &self,
        quote: Quote,
        request: AcceptQuoteRequest,
    ) -> Result<SettlementIntent, RouterError> {
        // Buy and sell diverge entirely at accept: buy finalizes the maker's
        // RGB-side PSBT now, sell only parks the BTC reservation and waits for
        // the taker's consignment. Dispatch and let each path run on its own.
        let (rgb_invoice, btc_funding_addr) = match &request.leg {
            SwapLeg::Buy {
                rgb_invoice,
                btc_funding_addr,
            } => (rgb_invoice.clone(), btc_funding_addr.clone()),
            SwapLeg::Sell {
                btc_payout_addr,
                rgb_change_invoice,
            } => {
                return self
                    .accept_quote_sell(quote, btc_payout_addr.clone(), rgb_change_invoice.clone())
                    .await;
            }
        };

        let now = now_ms();
        self.store.release_expired_reservations(now).await;

        let reservation_id = self
            .store
            .find_reservation_for_quote(&quote.quote_id)
            .await
            .ok_or_else(|| {
                RouterError::Maker("quote reservation not found or expired".to_owned())
            })?;

        // Reserved UTXOs for this quote: their outpoints feed the swap PSBT,
        // and their summed amount minus the quote amount is the maker's RGB
        // change. `Reserved` is the only status still carrying reservation_id.
        let reserved: Vec<InventoryUtxo> = self
            .store
            .list_for_asset(&quote.base_asset)
            .await
            .into_iter()
            .filter(|u| {
                matches!(&u.status,
                    InventoryStatus::Reserved { reservation_id: rid, .. } if rid == &reservation_id)
            })
            .collect();
        let reserved_outpoints: Vec<Outpoint> =
            reserved.iter().map(|u| u.outpoint.clone()).collect();

        // Declared-funding: discover the taker's BTC UTXOs from the address it
        // put on the ACCEPT, then select enough to cover the price + fee. The
        // taker only signs these inputs at `/sign`; it never adds its own.
        let actual_fee = self.estimate_swap_fee(&quote.base_asset.network).await;
        if quote.estimated_fee_sats > 0 {
            let cap = quote
                .estimated_fee_sats
                .saturating_mul(10_000 + u64::from(quote.fee_slippage_bps))
                / 10_000;
            if actual_fee > cap {
                let _ = self
                    .store
                    .release_reservation(&reservation_id, now_ms())
                    .await;
                return Err(RouterError::FeeSlippageExceeded {
                    estimated: quote.estimated_fee_sats,
                    actual: actual_fee,
                });
            }
        }
        // The witness-vout RGB receive output the maker adds costs SEAL_ANCHOR_SATS,
        // funded from the taker's inputs — so cover gross + fee + anchor, not just
        // gross + fee, or the assembled PSBT overspends (the taker's signer then
        // rejects it with "Outputs spends more than inputs amount").
        let required_funding = quote
            .price
            .saturating_add(actual_fee)
            .saturating_add(rfq_rgb::SEAL_ANCHOR_SATS);
        let taker_btc_inputs = match self.bitcoin_client.list_unspent(&btc_funding_addr).await {
            Ok(utxos) => {
                let available: u64 = utxos.iter().map(|(_, t)| t.value_sats).sum();
                match select_btc_inputs(&utxos, required_funding) {
                    Some(selected) => selected,
                    None => {
                        let _ = self
                            .store
                            .release_reservation(&reservation_id, now_ms())
                            .await;
                        // Surface the ACTUAL balance (0 when the keychain-0 address is
                        // unfunded) vs. what's needed — and log it maker-side so it's
                        // visible in `maker up`.
                        let msg = format!(
                            "buy declined: taker funding address {btc_funding_addr} has {available} sats, \
                             needs {required_funding} (price {} + fee {actual_fee} + anchor {}) — \
                             fund this keychain-0 address",
                            quote.price, rfq_rgb::SEAL_ANCHOR_SATS,
                        );
                        eprintln!("{msg}");
                        return Err(RouterError::Maker(msg));
                    }
                }
            }
            Err(e) => {
                let _ = self
                    .store
                    .release_reservation(&reservation_id, now_ms())
                    .await;
                let msg = format!("list_unspent({btc_funding_addr}) failed: {e}");
                eprintln!("buy accept error: {msg}");
                return Err(RouterError::Maker(msg));
            }
        };

        // Settlement-piggyback: if a ladder is configured, split the maker's RGB
        // change into rungs riding this swap (empty otherwise → ordinary change).
        let change_rungs = self
            .compute_buy_change_rungs(&quote.base_asset, &reserved, quote.amount)
            .await;

        // Build the maker-RGB-side PSBT + consignment. On failure the
        // reservation goes back to Available.
        let transfer = match self
            .rgb_backend
            .create_swap_psbt_buy(
                &rgb_invoice,
                quote.amount,
                &reserved_outpoints,
                &taker_btc_inputs,
                &btc_funding_addr,
                quote.price,
                actual_fee,
                &change_rungs,
            )
            .await
        {
            Ok(t) => t,
            Err(e) => {
                let _ = self
                    .store
                    .release_reservation(&reservation_id, now_ms())
                    .await;
                return Err(RouterError::Maker(e.to_string()));
            }
        };

        // The reservation carries a 30s quote TTL; extend it to the settlement
        // window so the cleanup loop doesn't release it mid-signature.
        let expires_at_ms = now_ms() + TAKER_SIGNATURE_TTL_MS;
        self.store
            .extend_reservation(&reservation_id, expires_at_ms, now_ms())
            .await
            .map_err(|e| RouterError::Maker(e.to_string()))?;

        // Stash the per-quote state `submit_signed_psbt` needs. A buy-side
        // transfer always carries `Some(consignment)`; default defensively.
        self.pending.write().await.insert(
            quote.quote_id.clone(),
            PendingSettlement::Buy(PendingBuySettlement {
                quote: quote.clone(),
                reservation_id: reservation_id.clone(),
                consignment: transfer.consignment.clone().unwrap_or_default(),
            }),
        );

        Ok(SettlementIntent {
            quote_id: quote.quote_id,
            maker_id: self.maker_id.clone(),
            status: SettlementStatus::AwaitingTakerSignature,
            transfer: Some(transfer),
            expires_at_ms,
            witness_txid: None,
            final_consignment: None,
        })
    }

    async fn submit_signed_psbt(
        &self,
        quote_id: QuoteId,
        signed_psbt_base64: String,
    ) -> Result<SettlementIntent, RouterError> {
        self.store.release_expired_reservations(now_ms()).await;
        self.btc_store.release_expired_reservations(now_ms()).await;

        let pending = self
            .pending
            .read()
            .await
            .get(&quote_id)
            .cloned()
            .ok_or_else(|| RouterError::Maker("no pending settlement for quote".to_owned()))?;

        // `/sign` is shared across both swap directions — hand a sell-side
        // settlement to its own path; the rest of this method is buy-side.
        let pending = match pending {
            PendingSettlement::Buy(p) => p,
            PendingSettlement::Sell(p) => {
                return self
                    .submit_signed_psbt_sell(quote_id, p, signed_psbt_base64)
                    .await;
            }
        };

        // The reservation must still be live. If the cleanup loop already
        // released it (taker took longer than TAKER_SIGNATURE_TTL_MS), the
        // settlement is dead — drop the stale entry and report it.
        if self
            .store
            .find_reservation_for_quote(&quote_id)
            .await
            .is_none()
        {
            self.pending.write().await.remove(&quote_id);
            return Err(RouterError::Maker(
                "settlement expired before signature was submitted".to_owned(),
            ));
        }

        let reservation_id = pending.reservation_id.clone();
        let _quote = &pending.quote;

        // No fee-slippage guard here: under declared-funding the buy PSBT is
        // fully committed at accept time (every input is known then), so the
        // fee was locked and slippage-checked in `accept_quote`. The taker only
        // adds signatures at `/sign` — it can't change the fee.

        // Finalize the taker-signed PSBT into a broadcastable tx.
        let finalized = match self
            .rgb_backend
            .finalize_after_taker_sign(&signed_psbt_base64, &pending.consignment)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                let _ = self
                    .store
                    .mark_broadcast_failed(&reservation_id, now_ms())
                    .await;
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::PsbtInvalid(e.to_string()));
            }
        };

        // Broadcast. A failure here releases the reservation — the swap tx
        // never hit the network, so the maker's UTXOs are still spendable.
        if let Err(e) = self.bitcoin_client.broadcast(&finalized.raw_tx).await {
            let _ = self
                .store
                .mark_broadcast_failed(&reservation_id, now_ms())
                .await;
            self.pending.write().await.remove(&quote_id);
            return Err(RouterError::Maker(format!("broadcast failed: {e}")));
        }

        // Tx is on the wire — move the reserved UTXOs to PendingBitcoinConfirm.
        // The RGB change UTXO from this swap is *not* ingested here: the chain
        // observer's `ingest_rgb_change_utxos` does that with status `Available`
        // once `sync_wallet` sees the new outpoint, mirroring the BTC change
        // path. Pre-emptive ingestion was tried in #14e but left change stuck
        // in `PendingBitcoinConfirm` forever (no transition back to Available).
        self.store
            .mark_pending_bitcoin_confirm(&reservation_id, finalized.witness_txid.clone(), now_ms())
            .await
            .map_err(|e| RouterError::Maker(e.to_string()))?;

        // Tx is broadcast — persist the consignment so it can be re-served if the
        // taker loses theirs (buy: the maker→taker token transfer).
        self.persist_consignment(
            &quote_id,
            &pending.quote.base_asset.id,
            &finalized.witness_txid,
            &finalized.final_consignment_base64,
        )
        .await;

        // Record the fill (FILLED counter + auto-mirror work-list).
        self.record_fill(&pending.quote, &finalized.witness_txid)
            .await;

        self.pending.write().await.remove(&quote_id);

        Ok(SettlementIntent {
            quote_id,
            maker_id: self.maker_id.clone(),
            status: SettlementStatus::PendingBitcoinConfirm,
            transfer: None,
            expires_at_ms: now_ms() + BROADCAST_CONFIRM_TTL_MS,
            witness_txid: Some(finalized.witness_txid),
            final_consignment: Some(finalized.final_consignment_base64),
        })
    }

    async fn deliver_consignment(
        &self,
        quote_id: QuoteId,
        consignment_base64: String,
        consigned_outpoints: Vec<Outpoint>,
    ) -> Result<SettlementIntent, RouterError> {
        self.deliver_consignment_sell(quote_id, consignment_base64, consigned_outpoints)
            .await
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// "Is this witness tx on chain?" probe used by `Maker::sweep_confirmations`
/// without adding a new method to the `BitcoinClient` trait. Queries
/// `(witness_txid, 0)` via `get_outpoint`: under tapret vout 0 is the maker's
/// P2TR commitment-host output, so a confirmed tx replies `Ok` (its UTXO is
/// live) or `OutpointNotFound` (already spent) — both mean "on chain". A
/// still-missing tx replies with a `Backend` error (typically "transaction not
/// found") and reads as "not yet". The legacy `NonSegwitOutpoint` arm still
/// covers an opret `OP_RETURN` host, so the probe works under either close
/// method. Anything else (connection failure, etc.) reads as "not yet" —
/// harmless, the next sweep tick retries.
async fn tx_confirmed(client: &dyn BitcoinClient, witness_txid: &str) -> bool {
    use rfq_btc::BtcError;
    use rfq_types::Outpoint;
    let probe = Outpoint::new(witness_txid.to_owned(), 0);
    match client.get_outpoint(&probe).await {
        Ok(_) => true,
        Err(BtcError::NonSegwitOutpoint(_)) => true,
        Err(BtcError::OutpointNotFound(_)) => true,
        Err(_) => false,
    }
}

/// Greedy largest-first selection over the taker's declared-funding UTXOs.
/// Returns the chosen `(outpoint, prevout)` pairs once their values cover
/// `target_sats`, or `None` if the address can't. Mirrors
/// `GreedyLargestFirstSelector`, but operates on the `(Outpoint, TxOut)` shape
/// `BitcoinClient::list_unspent` returns (taker UTXOs aren't maker inventory,
/// so they don't pass through `BtcInventoryStore`).
fn select_btc_inputs(
    available: &[(Outpoint, TxOut)],
    target_sats: u64,
) -> Option<Vec<(Outpoint, TxOut)>> {
    let mut sorted: Vec<&(Outpoint, TxOut)> = available.iter().collect();
    // Largest first; tie-break on outpoint for determinism.
    sorted.sort_by(|a, b| {
        b.1.value_sats
            .cmp(&a.1.value_sats)
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut chosen = Vec::new();
    let mut total = 0u64;
    for pair in sorted {
        if total >= target_sats {
            break;
        }
        total = total.saturating_add(pair.1.value_sats);
        chosen.push(pair.clone());
    }
    (total >= target_sats).then_some(chosen)
}

#[cfg(test)]
mod tests;
