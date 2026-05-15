use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use rfq_btc::BitcoinClient;
use rfq_rgb::RgbBackend;
use rfq_router::{MakerConnector, RouterError};
use rfq_store::{InMemoryInventoryStore, InventoryStore};
use rfq_types::{
    AcceptQuoteRequest, AssetId, ExtendedInventorySnapshot, InventoryError, InventorySnapshot,
    InventoryStatus, InventoryUtxo, MakerId, Outpoint, Quote, QuoteId, QuoteRequest,
    RgbInventoryUtxo, ReservationId, SettlementIntent, SettlementStatus, SwapLeg,
};
use tokio::sync::RwLock;
use uuid::Uuid;

mod coin_select;
pub use coin_select::{CoinSelectionError, CoinSelector, GreedyExactFitSelector, Selection};

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
/// Upper bound on reservation retries under contention. With outpoint
/// exclusion on each retry, the effective bound is `min(this, available_utxo_count)`
/// — the loop exits via the selector's Insufficient branch once exclusions
/// have whittled the candidate set to empty.
const RESERVE_RETRY_ATTEMPTS: u32 = 16;

/// Maker-side state for a buy-side swap held between `accept_quote` and
/// `submit_signed_psbt`. The settlement state machine (#9) will formalize
/// this; for 15c a per-quote map keyed by `QuoteId` is enough.
#[derive(Clone)]
struct PendingBuySettlement {
    quote: Quote,
    reservation_id: ReservationId,
    /// Maker-built consignment handed to the taker at accept; needed again to
    /// finalize once the taker returns the signed PSBT.
    consignment: String,
    /// Sats the maker over-selected — re-ingested as a change UTXO post-broadcast.
    expected_change: u64,
}

#[derive(Clone)]
pub struct MockMaker {
    maker_id: MakerId,
    store: Arc<dyn InventoryStore>,
    selector: Arc<dyn CoinSelector>,
    rgb_backend: Arc<dyn RgbBackend>,
    bitcoin_client: Arc<dyn BitcoinClient>,
    pending: Arc<RwLock<HashMap<QuoteId, PendingBuySettlement>>>,
}

impl MockMaker {
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
    /// store needs to be shared across components.
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
            rgb_backend,
            bitcoin_client,
            pending: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Per-UTXO view across all assets. Returned in outpoint order so callers
    /// can index deterministically.
    pub async fn utxo_snapshot(&self) -> Vec<InventoryUtxo> {
        let mut utxos = self.store.list_all().await;
        utxos.sort_by(|a, b| a.outpoint.cmp(&b.outpoint));
        utxos
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

    pub async fn release_expired_reservations(&self) -> usize {
        let released = self.store.release_expired_reservations(now_ms()).await;
        // Prune buy-settlement state whose reservation no longer exists —
        // either expired above, or already transitioned out of `Reserved` by
        // a completed `submit_signed_psbt`. `find_reservation_for_quote` only
        // matches `Reserved`, so a settled quote reads as "gone" here, which
        // is fine: `submit_signed_psbt` already removed its own entry.
        let quote_ids: Vec<QuoteId> = self.pending.read().await.keys().cloned().collect();
        let mut stale = Vec::new();
        for quote_id in quote_ids {
            if self
                .store
                .find_reservation_for_quote(&quote_id)
                .await
                .is_none()
            {
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
}

#[async_trait]
impl MakerConnector for MockMaker {
    fn maker_id(&self) -> MakerId {
        self.maker_id.clone()
    }

    async fn request_quote(&self, request: QuoteRequest) -> Result<Option<Quote>, RouterError> {
        let now = now_ms();
        self.store.release_expired_reservations(now).await;

        let quote_id = QuoteId(Uuid::new_v4().to_string());
        let expires_at_ms = now + QUOTE_TTL_MS;

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
            let selection = match self
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

        // Quote-time fee estimate. A failed estimate falls back to 0, which
        // disables the slippage check in `submit_signed_psbt` (can't compare
        // against an unknown baseline) rather than rejecting every quote.
        let feerate = self.bitcoin_client.estimate_feerate(3).await.unwrap_or(0);
        let estimated_fee_sats = feerate.saturating_mul(ESTIMATED_SWAP_VBYTES);

        Ok(Some(Quote {
            quote_id,
            rfq_id: request.rfq_id,
            maker_id: self.maker_id.clone(),
            base_asset: request.base_asset,
            quote_asset: request.quote_asset,
            side: request.side,
            amount: selection.requested,
            price: selection.requested.saturating_mul(101),
            expires_at_ms,
            estimated_fee_sats,
            // 20% slippage cap — the v0 default per docs/swap-flows.md.
            fee_slippage_bps: 2000,
            // Set on sell-side quotes only — 16c populates this with a maker
            // invoice. Buy-side quotes always carry None.
            maker_rgb_invoice: None,
        }))
    }

    async fn accept_quote(
        &self,
        quote: Quote,
        request: AcceptQuoteRequest,
    ) -> Result<SettlementIntent, RouterError> {
        // 15a pattern-matches the side. Sell side returns the typed "not yet
        // supported" error; 16c fills in the body. Buy side runs the existing
        // 14e behavior with a relabelled return status.
        let rgb_invoice = match &request.leg {
            SwapLeg::Buy { rgb_invoice } => rgb_invoice.clone(),
            SwapLeg::Sell { .. } => {
                return Err(RouterError::Maker(
                    "sell side not yet supported".to_owned(),
                ));
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
        let reserved_total: u64 = reserved.iter().map(|u| u.amount).sum();
        let reserved_outpoints: Vec<Outpoint> =
            reserved.iter().map(|u| u.outpoint.clone()).collect();
        let expected_change = reserved_total.saturating_sub(quote.amount);

        // Build the maker-RGB-side PSBT + consignment. On failure the
        // reservation goes back to Available.
        let transfer = match self
            .rgb_backend
            .create_swap_psbt_buy(&rgb_invoice, quote.amount, &reserved_outpoints)
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
            PendingBuySettlement {
                quote: quote.clone(),
                reservation_id: reservation_id.clone(),
                consignment: transfer.consignment.clone().unwrap_or_default(),
                expected_change,
            },
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

        let pending = self
            .pending
            .read()
            .await
            .get(&quote_id)
            .cloned()
            .ok_or_else(|| RouterError::Maker("no pending settlement for quote".to_owned()))?;

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
        let quote = &pending.quote;

        // Fee-slippage guard. Skipped when the quote-time estimate was 0 (the
        // feerate estimate failed at quote time — no baseline to compare).
        if quote.estimated_fee_sats > 0 {
            let feerate = self
                .bitcoin_client
                .estimate_feerate(3)
                .await
                .map_err(|e| RouterError::Maker(format!("feerate estimate failed: {e}")))?;
            let actual_fee = feerate.saturating_mul(ESTIMATED_SWAP_VBYTES);
            let cap = quote
                .estimated_fee_sats
                .saturating_mul(10_000 + u64::from(quote.fee_slippage_bps))
                / 10_000;
            if actual_fee > cap {
                let _ = self
                    .store
                    .mark_broadcast_failed(&reservation_id, now_ms())
                    .await;
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::FeeSlippageExceeded {
                    estimated: quote.estimated_fee_sats,
                    actual: actual_fee,
                });
            }
        }

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
        self.store
            .mark_pending_bitcoin_confirm(
                &reservation_id,
                finalized.witness_txid.clone(),
                now_ms(),
            )
            .await
            .map_err(|e| RouterError::Maker(e.to_string()))?;

        // Change re-ingestion: the broadcast tx produces an RGB change output
        // back to the maker when the selection over-shot. Best-effort — an
        // ingestion collision doesn't undo a settled swap.
        if pending.expected_change > 0 {
            let now = now_ms();
            let change_utxo = InventoryUtxo {
                outpoint: Outpoint::new(finalized.witness_txid.clone(), 1),
                asset_id: quote.base_asset.clone(),
                amount: pending.expected_change,
                btc_sats: 0,
                status: InventoryStatus::PendingBitcoinConfirm {
                    reservation_id: reservation_id.clone(),
                    witness_txid: finalized.witness_txid.clone(),
                },
                created_at_ms: now,
                updated_at_ms: now,
                pending_txid: Some(finalized.witness_txid.clone()),
            };
            if let Err(e) = self.store.ingest_change_utxo(change_utxo).await {
                eprintln!("change re-ingestion failed (continuing): {e}");
            }
        }

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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_btc::MockBitcoinClient;
    use rfq_rgb::MockRgbBackend;
    use rfq_types::{AssetId, AssetKind, BitcoinNetwork, ReservationId, RfqId, Side};

    fn synth_outpoint(idx: usize) -> Outpoint {
        Outpoint::new(format!("{idx:064x}"), 0)
    }

    fn maker_id() -> MakerId {
        MakerId("maker-1".to_owned())
    }

    fn asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-test-asset".to_owned(),
        }
    }

    fn quote_asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Btc,
            id: "btc".to_owned(),
        }
    }

    fn utxo_with_amount(idx: usize, amount: u64) -> RgbInventoryUtxo {
        RgbInventoryUtxo {
            outpoint: synth_outpoint(idx),
            asset_id: asset(),
            amount,
            btc_sats: 0,
        }
    }

    fn utxo() -> RgbInventoryUtxo {
        utxo_with_amount(0, 100)
    }

    fn inv_utxo(idx: usize, amount: u64, status: InventoryStatus) -> InventoryUtxo {
        let now = now_ms();
        InventoryUtxo {
            outpoint: synth_outpoint(idx),
            asset_id: asset(),
            amount,
            btc_sats: 0,
            status,
            created_at_ms: now,
            updated_at_ms: now,
            pending_txid: None,
        }
    }

    fn maker() -> MockMaker {
        maker_with_utxos(vec![utxo()])
    }

    fn maker_with_utxos(utxos: Vec<RgbInventoryUtxo>) -> MockMaker {
        maker_with_btc(utxos, Arc::new(MockBitcoinClient::new()))
    }

    /// Like `maker_with_utxos` but lets a test hold the `MockBitcoinClient` it
    /// passed in, so it can drive broadcast-failure / feerate scenarios.
    fn maker_with_btc(
        utxos: Vec<RgbInventoryUtxo>,
        bitcoin_client: Arc<MockBitcoinClient>,
    ) -> MockMaker {
        let rgb_backend = Arc::new(MockRgbBackend::new(utxos.clone()));
        MockMaker::new(maker_id(), utxos, rgb_backend, bitcoin_client)
    }

    /// Seed the store directly so tests can pre-set Reserved / Spent statuses.
    fn maker_with_store(rows: Vec<InventoryUtxo>) -> MockMaker {
        let rgb_backend = Arc::new(MockRgbBackend::new(vec![utxo()]));
        MockMaker::with_components(
            maker_id(),
            Arc::new(InMemoryInventoryStore::with_seed(rows)),
            Arc::new(GreedyExactFitSelector),
            rgb_backend,
            Arc::new(MockBitcoinClient::new()),
        )
    }

    /// Build a `SwapLeg::Buy` accept request for `quote`.
    fn buy_request(quote: &Quote, invoice: &str) -> AcceptQuoteRequest {
        AcceptQuoteRequest {
            quote_id: quote.quote_id.clone(),
            leg: SwapLeg::Buy {
                rgb_invoice: invoice.to_owned(),
            },
        }
    }

    fn quote_request(id: &str) -> QuoteRequest {
        QuoteRequest {
            rfq_id: RfqId(id.to_owned()),
            base_asset: asset(),
            quote_asset: quote_asset(),
            side: Side::Buy,
            amount: 100,
            created_at_ms: now_ms(),
        }
    }

    #[tokio::test]
    async fn first_quote_reserves_allocation() {
        let maker = maker();

        let quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();

        let quote = quote.expect("quote should be returned");
        let utxos = maker.utxo_snapshot().await;
        assert_eq!(utxos.len(), 1);
        assert!(matches!(
            &utxos[0].status,
            InventoryStatus::Reserved { quote_id, expires_at_ms, .. }
                if quote_id == &quote.quote_id && *expires_at_ms == quote.expires_at_ms
        ));
    }

    #[tokio::test]
    async fn inventory_summary_counts_initial_inventory_as_available() {
        let maker = maker_with_utxos(vec![
            utxo_with_amount(0, 100),
            utxo_with_amount(1, 200),
        ]);

        let snapshot = maker.inventory_summary().await;

        assert_eq!(
            snapshot,
            InventorySnapshot {
                total_amount: 300,
                available_amount: 300,
                reserved_amount: 0,
                spent_amount: 0,
                total_allocations: 2,
                available_allocations: 2,
                reserved_allocations: 0,
                spent_allocations: 0,
            }
        );
    }

    #[tokio::test]
    async fn inventory_summary_counts_reserved_after_quote() {
        let maker = maker();

        let quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();
        let snapshot = maker.inventory_summary().await;

        assert!(quote.is_some());
        assert_eq!(snapshot.available_amount, 0);
        assert_eq!(snapshot.reserved_amount, 100);
        assert_eq!(snapshot.spent_amount, 0);
        assert_eq!(snapshot.available_allocations, 0);
        assert_eq!(snapshot.reserved_allocations, 1);
        assert_eq!(snapshot.spent_allocations, 0);
    }

    #[tokio::test]
    async fn inventory_summary_releases_expired_reservation() {
        let maker = maker_with_store(vec![inv_utxo(
            0,
            100,
            InventoryStatus::Reserved {
                reservation_id: ReservationId("res-0".to_owned()),
                rfq_id: RfqId("rfq-0".to_owned()),
                quote_id: QuoteId("expired-quote".to_owned()),
                expires_at_ms: 0,
            },
        )]);

        let snapshot = maker.inventory_summary().await;
        let utxos = maker.utxo_snapshot().await;

        assert_eq!(snapshot.available_amount, 100);
        assert_eq!(snapshot.reserved_amount, 0);
        assert_eq!(snapshot.available_allocations, 1);
        assert_eq!(snapshot.reserved_allocations, 0);
        assert!(matches!(utxos[0].status, InventoryStatus::Available));
    }

    #[tokio::test]
    async fn release_expired_reservations_returns_count_and_releases() {
        let maker = maker_with_store(vec![inv_utxo(
            0,
            100,
            InventoryStatus::Reserved {
                reservation_id: ReservationId("res-0".to_owned()),
                rfq_id: RfqId("rfq-0".to_owned()),
                quote_id: QuoteId("expired-quote".to_owned()),
                expires_at_ms: 0,
            },
        )]);

        let released = maker.release_expired_reservations().await;
        let utxos = maker.utxo_snapshot().await;

        assert_eq!(released, 1);
        assert!(matches!(utxos[0].status, InventoryStatus::Available));
    }

    #[tokio::test]
    async fn release_expired_reservations_ignores_active_and_spent() {
        let maker = maker_with_store(vec![
            inv_utxo(
                0,
                100,
                InventoryStatus::Reserved {
                    reservation_id: ReservationId("res-0".to_owned()),
                    rfq_id: RfqId("rfq-0".to_owned()),
                    quote_id: QuoteId("active-quote".to_owned()),
                    expires_at_ms: now_ms() + 30_000,
                },
            ),
            inv_utxo(
                1,
                100,
                InventoryStatus::Spent {
                    witness_txid: "wt-1".to_owned(),
                    quote_id: QuoteId("spent-quote".to_owned()),
                },
            ),
        ]);

        let released = maker.release_expired_reservations().await;
        let utxos = maker.utxo_snapshot().await;

        assert_eq!(released, 0);
        assert!(matches!(
            utxos[0].status,
            InventoryStatus::Reserved { .. }
        ));
        assert!(matches!(utxos[1].status, InventoryStatus::Spent { .. }));
    }

    #[tokio::test]
    async fn second_quote_cannot_reuse_reserved_liquidity() {
        let maker = maker();

        let first_quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();
        let second_quote = maker.request_quote(quote_request("rfq-2")).await.unwrap();

        assert!(first_quote.is_some());
        assert!(second_quote.is_none());
    }

    #[tokio::test]
    async fn expired_reservation_becomes_available() {
        let expired_quote_id = QuoteId("expired-quote".to_owned());
        let maker = maker_with_store(vec![inv_utxo(
            0,
            100,
            InventoryStatus::Reserved {
                reservation_id: ReservationId("res-0".to_owned()),
                rfq_id: RfqId("rfq-0".to_owned()),
                quote_id: expired_quote_id,
                expires_at_ms: 0,
            },
        )]);

        let quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();

        assert!(quote.is_some());
        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(
            &utxos[0].status,
            InventoryStatus::Reserved { quote_id, .. } if quote_id == &quote.unwrap().quote_id
        ));
    }

    #[tokio::test]
    async fn accept_keeps_utxo_reserved_awaiting_signature() {
        // Under the swap flow, `accept` no longer spends — it builds the PSBT
        // and waits for the taker's signature. The reserved UTXO stays
        // `Reserved` (with its deadline extended to the settlement window).
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote should be returned");

        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:test_invoice"))
            .await
            .unwrap();

        assert_eq!(intent.status, SettlementStatus::AwaitingTakerSignature);
        assert!(intent.transfer.is_some());
        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(
            &utxos[0].status,
            InventoryStatus::Reserved { quote_id, .. } if quote_id == &quote.quote_id
        ));
    }

    #[tokio::test]
    async fn inventory_counts_pending_confirm_after_sign() {
        // accept → sign drives the input UTXO to `PendingBitcoinConfirm`
        // (broadcast, awaiting confirmation) — not `Spent`, which only happens
        // once the witness tx confirms.
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote should be returned");

        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:test_invoice"))
            .await
            .unwrap();
        let psbt = intent.transfer.expect("transfer").partial_psbt;
        maker
            .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
            .await
            .unwrap();

        let ext = maker.extended_inventory_summary().await;
        assert_eq!(ext.available_utxos, 0);
        assert_eq!(ext.reserved_utxos, 0);
        assert_eq!(ext.pending_settlement_utxos, 1);
        assert_eq!(ext.pending_settlement_amount, 100);
    }

    #[tokio::test]
    async fn spent_allocation_cannot_be_quoted_again() {
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote should be returned");
        maker
            .accept_quote(
                quote.clone(),
                AcceptQuoteRequest {
                    quote_id: quote.quote_id.clone(),
                    leg: SwapLeg::Buy {
                        rgb_invoice: "rgb:test_invoice".to_owned(),
                    },
                },
            )
            .await
            .unwrap();

        let next_quote = maker.request_quote(quote_request("rfq-2")).await.unwrap();

        assert!(next_quote.is_none());
    }

    // --- new 14c tests ---

    #[tokio::test]
    async fn request_quote_reserves_specific_utxos_not_whole_inventory() {
        // Three available UTXOs of 100 each. A request for 80 reserves exactly
        // one of them — the other two stay Available. This is the per-UTXO
        // win over the pre-#14 whole-allocation model.
        let maker = maker_with_utxos(vec![
            utxo_with_amount(0, 100),
            utxo_with_amount(1, 100),
            utxo_with_amount(2, 100),
        ]);

        let mut request = quote_request("rfq-1");
        request.amount = 80;
        let quote = maker.request_quote(request).await.unwrap();
        assert!(quote.is_some());

        let ext = maker.extended_inventory_summary().await;
        assert_eq!(ext.available_utxos, 2);
        assert_eq!(ext.reserved_utxos, 1);
        assert_eq!(ext.available_amount, 200);
        assert_eq!(ext.reserved_amount, 100);
    }

    #[tokio::test]
    async fn expired_reservation_releases_per_utxo() {
        let maker = maker_with_store(vec![
            inv_utxo(
                0,
                100,
                InventoryStatus::Reserved {
                    reservation_id: ReservationId("res-0".to_owned()),
                    rfq_id: RfqId("rfq-0".to_owned()),
                    quote_id: QuoteId("expired".to_owned()),
                    expires_at_ms: 0,
                },
            ),
            inv_utxo(1, 200, InventoryStatus::Available),
        ]);

        let released = maker.release_expired_reservations().await;
        let ext = maker.extended_inventory_summary().await;

        assert_eq!(released, 1);
        assert_eq!(ext.available_utxos, 2);
        assert_eq!(ext.reserved_utxos, 0);
    }

    #[tokio::test]
    async fn concurrent_request_quotes_do_not_double_reserve() {
        // Five UTXOs of 100 each. Spawn 10 concurrent quote requests of 50;
        // at most 5 should succeed (one UTXO per quote with WholeUtxoSelector).
        let maker = Arc::new(maker_with_utxos(vec![
            utxo_with_amount(0, 100),
            utxo_with_amount(1, 100),
            utxo_with_amount(2, 100),
            utxo_with_amount(3, 100),
            utxo_with_amount(4, 100),
        ]));

        let mut handles = Vec::new();
        for i in 0..10 {
            let maker = maker.clone();
            handles.push(tokio::spawn(async move {
                let mut request = quote_request(&format!("rfq-{i}"));
                request.amount = 50;
                maker.request_quote(request).await.unwrap()
            }));
        }

        let mut successes = 0;
        for h in handles {
            if h.await.unwrap().is_some() {
                successes += 1;
            }
        }
        assert_eq!(successes, 5);

        let ext = maker.extended_inventory_summary().await;
        assert_eq!(ext.reserved_utxos, 5);
        assert_eq!(ext.available_utxos, 0);
    }

    // --- 14e tests ---

    #[tokio::test]
    async fn rebalance_emits_no_triggers_for_healthy_inventory() {
        // Single big UTXO → fragmentation 0.0, available_utxos = 1 (between
        // min and max). All triggers should be silent.
        let maker = maker_with_utxos(vec![utxo_with_amount(0, 1000)]);
        let policy = RebalancePolicy {
            fragmentation_threshold: 0.7,
            max_utxo_count: 50,
            min_utxo_count: 1,
        };
        let plan = maker.rebalance(&policy).await;
        assert!(plan.is_empty(), "expected no triggers, got {plan:?}");
    }

    #[tokio::test]
    async fn rebalance_fires_high_fragmentation_above_threshold() {
        // 5 UTXOs of 100 each. Largest 100 / total 500 = 0.2 share →
        // fragmentation = 0.8, which crosses 0.7.
        let maker = maker_with_utxos(vec![
            utxo_with_amount(0, 100),
            utxo_with_amount(1, 100),
            utxo_with_amount(2, 100),
            utxo_with_amount(3, 100),
            utxo_with_amount(4, 100),
        ]);
        let policy = RebalancePolicy {
            fragmentation_threshold: 0.7,
            max_utxo_count: 50,
            min_utxo_count: 1,
        };
        let plan = maker.rebalance(&policy).await;
        assert!(plan
            .triggers
            .iter()
            .any(|t| matches!(t, RebalanceTrigger::HighFragmentation { .. })));
    }

    #[tokio::test]
    async fn rebalance_fires_too_many_utxos_above_max() {
        let utxos: Vec<_> = (0..10).map(|i| utxo_with_amount(i, 10)).collect();
        let maker = maker_with_utxos(utxos);
        let policy = RebalancePolicy {
            fragmentation_threshold: 1.0, // suppress fragmentation trigger
            max_utxo_count: 5,
            min_utxo_count: 1,
        };
        let plan = maker.rebalance(&policy).await;
        assert!(plan.triggers.iter().any(|t| matches!(
            t,
            RebalanceTrigger::TooManyUtxos { count: 10, max: 5 }
        )));
    }

    #[tokio::test]
    async fn rebalance_fires_too_few_utxos_below_min() {
        let maker = maker_with_utxos(vec![utxo_with_amount(0, 1000)]);
        let policy = RebalancePolicy {
            fragmentation_threshold: 1.0,
            max_utxo_count: 50,
            min_utxo_count: 3,
        };
        let plan = maker.rebalance(&policy).await;
        assert!(plan.triggers.iter().any(|t| matches!(
            t,
            RebalanceTrigger::TooFewUtxos { count: 1, min: 3 }
        )));
    }

    #[tokio::test]
    async fn sign_with_change_ingests_pending_bitcoin_confirm_utxo() {
        // Single UTXO of 100; request 60 → change = 40. Change re-ingestion
        // happens at `/sign` (post-broadcast), not at accept. After sign, the
        // input UTXO is PendingBitcoinConfirm and a synthetic change UTXO of
        // 40 has been ingested, also PendingBitcoinConfirm.
        let maker = maker_with_utxos(vec![utxo_with_amount(0, 100)]);

        let mut request = quote_request("rfq-1");
        request.amount = 60;
        let quote = maker
            .request_quote(request)
            .await
            .unwrap()
            .expect("quote should be issued");

        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:test"))
            .await
            .unwrap();
        let psbt = intent.transfer.expect("transfer").partial_psbt;
        maker
            .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
            .await
            .unwrap();

        let utxos = maker.utxo_snapshot().await;
        let pending: Vec<_> = utxos
            .iter()
            .filter(|u| matches!(u.status, InventoryStatus::PendingBitcoinConfirm { .. }))
            .collect();
        // Input (100) + change (40), both PendingBitcoinConfirm.
        assert_eq!(pending.len(), 2);
        let change = pending
            .iter()
            .find(|u| u.amount == 40)
            .expect("expected a change UTXO");
        assert!(change.pending_txid.is_some());
    }

    #[tokio::test]
    async fn buy_flow_settles_through_accept_then_sign() {
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote should be returned");

        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
            .await
            .unwrap();
        assert_eq!(intent.status, SettlementStatus::AwaitingTakerSignature);
        let transfer = intent.transfer.expect("transfer");
        assert!(transfer.consignment.is_some());

        let final_intent = maker
            .submit_signed_psbt(
                quote.quote_id.clone(),
                format!("{}|signed", transfer.partial_psbt),
            )
            .await
            .unwrap();
        assert_eq!(final_intent.status, SettlementStatus::PendingBitcoinConfirm);
        assert!(final_intent.witness_txid.is_some());
        assert!(final_intent.final_consignment.is_some());

        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(
            utxos[0].status,
            InventoryStatus::PendingBitcoinConfirm { .. }
        ));
    }

    #[tokio::test]
    async fn submit_signed_psbt_unknown_quote_errors() {
        let maker = maker();
        let result = maker
            .submit_signed_psbt(QuoteId("no-such-quote".to_owned()), "psbt".to_owned())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn submit_signed_psbt_rejects_unsigned_psbt() {
        // MockRgbBackend::finalize_after_taker_sign rejects an empty PSBT.
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote");
        maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
            .await
            .unwrap();

        let result = maker
            .submit_signed_psbt(quote.quote_id.clone(), String::new())
            .await;
        assert!(matches!(result, Err(RouterError::PsbtInvalid(_))));
        // Reservation released back to Available.
        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(utxos[0].status, InventoryStatus::Available));
    }

    #[tokio::test]
    async fn submit_signed_psbt_fee_slippage_releases_reservation() {
        // Default mock feerate (5 sat/vB) × 200 vB = 1000 sat actual. Crafting
        // the accepted quote with a tiny baseline + zero slippage tolerance
        // makes the submit-time re-estimate blow the cap.
        let maker = maker();
        let mut quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote");
        quote.estimated_fee_sats = 1;
        quote.fee_slippage_bps = 0;

        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
            .await
            .unwrap();
        let psbt = intent.transfer.expect("transfer").partial_psbt;

        let result = maker
            .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
            .await;
        assert!(matches!(
            result,
            Err(RouterError::FeeSlippageExceeded { .. })
        ));
        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(utxos[0].status, InventoryStatus::Available));
    }

    #[tokio::test]
    async fn submit_signed_psbt_broadcast_failure_releases_reservation() {
        let btc = Arc::new(MockBitcoinClient::new());
        let maker = maker_with_btc(vec![utxo()], btc.clone());
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote");
        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
            .await
            .unwrap();
        let psbt = intent.transfer.expect("transfer").partial_psbt;

        btc.fail_next_broadcast("simulated rpc failure");
        let result = maker
            .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
            .await;
        assert!(result.is_err());
        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(utxos[0].status, InventoryStatus::Available));
    }

    #[tokio::test]
    async fn submit_signed_psbt_after_settlement_window_lapse_errors() {
        // Simulate the taker missing the TAKER_SIGNATURE_TTL_MS window: once
        // the cleanup loop has released the reservation, submit_signed_psbt
        // must refuse, drop the stale pending entry, and leave the maker's
        // UTXO back on Available — no broadcast.
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote");
        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
            .await
            .unwrap();
        let psbt = intent.transfer.expect("transfer").partial_psbt;

        // Stand in for the cleanup loop expiring the settlement window.
        let reservation_id = maker
            .store
            .find_reservation_for_quote(&quote.quote_id)
            .await
            .expect("reservation live after accept");
        maker
            .store
            .release_reservation(&reservation_id, now_ms())
            .await
            .unwrap();

        let result = maker
            .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
            .await;
        assert!(result.is_err());
        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(utxos[0].status, InventoryStatus::Available));
    }

    #[tokio::test]
    async fn accept_releases_reservation_on_transfer_failure() {
        // MockRgbBackend rejects invoices that don't start with "rgb:".
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote should be issued");

        let result = maker
            .accept_quote(
                quote.clone(),
                AcceptQuoteRequest {
                    quote_id: quote.quote_id,
                    leg: SwapLeg::Buy {
                        rgb_invoice: "not-an-invoice".into(),
                    },
                },
            )
            .await;
        assert!(result.is_err());

        let snapshot = maker.inventory_summary().await;
        assert_eq!(snapshot.available_amount, 100);
        assert_eq!(snapshot.reserved_amount, 0);
        assert_eq!(snapshot.spent_amount, 0);
    }

    #[tokio::test]
    async fn legacy_inventory_summary_matches_extended_downcast() {
        let maker = maker_with_store(vec![
            inv_utxo(0, 100, InventoryStatus::Available),
            inv_utxo(
                1,
                200,
                InventoryStatus::Reserved {
                    reservation_id: ReservationId("res-1".to_owned()),
                    rfq_id: RfqId("rfq-1".to_owned()),
                    quote_id: QuoteId("q".to_owned()),
                    expires_at_ms: now_ms() + 60_000,
                },
            ),
        ]);

        let legacy = maker.inventory_summary().await;
        let ext = maker.extended_inventory_summary().await;
        let derived: InventorySnapshot = (&ext).into();
        assert_eq!(legacy, derived);
    }
}
