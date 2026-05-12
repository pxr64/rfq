use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use rfq_rgb::RgbBackend;
use rfq_router::{MakerConnector, RouterError};
use rfq_store::{InMemoryInventoryStore, InventoryStore};
use rfq_types::{
    AcceptQuoteRequest, Allocation, AllocationState, AssetId, ExtendedInventorySnapshot,
    InventoryError, InventorySnapshot, InventoryStatus, InventoryUtxo, MakerId, ManagedAllocation,
    Outpoint, Quote, QuoteId, QuoteRequest, ReservationId, RfqId, SettlementIntent,
    SettlementStatus,
};
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
/// Upper bound on reservation retries under contention. With outpoint
/// exclusion on each retry, the effective bound is `min(this, available_utxo_count)`
/// — the loop exits via the selector's Insufficient branch once exclusions
/// have whittled the candidate set to empty.
const RESERVE_RETRY_ATTEMPTS: u32 = 16;

#[derive(Clone)]
pub struct MockMaker {
    maker_id: MakerId,
    store: Arc<dyn InventoryStore>,
    selector: Arc<dyn CoinSelector>,
    rgb_backend: Arc<dyn RgbBackend>,
}

impl MockMaker {
    /// Backward-compatible constructor: seeds the inventory store with one
    /// synthetic outpoint per `Allocation`. Used by call sites that haven't
    /// migrated to building their own `InventoryUtxo` set yet.
    pub fn new(
        maker_id: MakerId,
        allocations: Vec<Allocation>,
        rgb_backend: Arc<dyn RgbBackend>,
    ) -> Self {
        let now = now_ms();
        let utxos: Vec<InventoryUtxo> = allocations
            .into_iter()
            .enumerate()
            .map(|(idx, a)| InventoryUtxo {
                outpoint: synth_outpoint(idx),
                asset_id: a.asset,
                amount: a.available_amount,
                btc_sats: 0,
                status: InventoryStatus::Available,
                created_at_ms: now,
                updated_at_ms: now,
                pending_txid: None,
            })
            .collect();
        Self::with_components(
            maker_id,
            Arc::new(InMemoryInventoryStore::with_seed(utxos)),
            Arc::new(GreedyExactFitSelector),
            rgb_backend,
        )
    }

    /// Seed from a `Vec<ManagedAllocation>` — used by legacy tests to pre-set
    /// Reserved / Spent states. Non-`Available` states get synthesized
    /// reservation_ids / rfq_ids / witness_txids so the conversion is total.
    pub fn new_with_inventory(
        maker_id: MakerId,
        inventory: Vec<ManagedAllocation>,
        rgb_backend: Arc<dyn RgbBackend>,
    ) -> Self {
        let now = now_ms();
        let utxos: Vec<InventoryUtxo> = inventory
            .into_iter()
            .enumerate()
            .map(|(idx, m)| InventoryUtxo {
                outpoint: synth_outpoint(idx),
                asset_id: m.allocation.asset,
                amount: m.allocation.available_amount,
                btc_sats: 0,
                status: legacy_state_to_inventory_status(idx, m.state),
                created_at_ms: now,
                updated_at_ms: now,
                pending_txid: None,
            })
            .collect();
        Self::with_components(
            maker_id,
            Arc::new(InMemoryInventoryStore::with_seed(utxos)),
            Arc::new(GreedyExactFitSelector),
            rgb_backend,
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
    ) -> Self {
        Self {
            maker_id,
            store,
            selector,
            rgb_backend,
        }
    }

    /// Legacy view: `Vec<ManagedAllocation>` sorted by outpoint. Internal
    /// pending/invalid states (introduced by 14e) are not representable in
    /// `AllocationState` and would panic; MockMaker doesn't transition into
    /// those states until 14e, so the conversion is total today.
    pub async fn inventory_snapshot(&self) -> Vec<ManagedAllocation> {
        let mut utxos = self.store.list_all().await;
        utxos.sort_by(|a, b| a.outpoint.cmp(&b.outpoint));
        let maker_id = self.maker_id.clone();
        utxos
            .into_iter()
            .map(|u| utxo_to_managed_allocation(u, maker_id.clone()))
            .collect()
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
        self.store.release_expired_reservations(now_ms()).await
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
        }))
    }

    async fn accept_quote(
        &self,
        quote: Quote,
        request: AcceptQuoteRequest,
    ) -> Result<SettlementIntent, RouterError> {
        let now = now_ms();
        self.store.release_expired_reservations(now).await;

        let reservation_id = self
            .store
            .find_reservation_for_quote(&quote.quote_id)
            .await
            .ok_or_else(|| {
                RouterError::Maker("quote reservation not found or expired".to_owned())
            })?;

        // Compute expected change before mark_spent transitions the
        // reservation. The Reserved variant is the only status that still
        // carries reservation_id at this point.
        let reserved_total: u64 = self
            .store
            .list_for_asset(&quote.base_asset)
            .await
            .into_iter()
            .filter_map(|u| match &u.status {
                InventoryStatus::Reserved { reservation_id: rid, .. } if rid == &reservation_id => {
                    Some(u.amount)
                }
                _ => None,
            })
            .sum();
        let expected_change = reserved_total.saturating_sub(quote.amount);

        let transfer = match self
            .rgb_backend
            .create_transfer(&request.rgb_invoice, quote.amount)
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

        // Synthesize a witness txid until #13 wires the real broadcast path.
        let witness_txid = format!("mock-wt-{}", quote.quote_id.0);
        self.store
            .mark_spent(&reservation_id, witness_txid.clone(), now_ms())
            .await
            .map_err(|e| RouterError::Maker(e.to_string()))?;

        // Change re-ingestion: when the selection had to take more than
        // requested, the broadcast tx would produce a change output back to
        // the maker. The mock synthesizes the outpoint (vout=1) and registers
        // it as PendingBitcoinConfirm. Best-effort — ingestion failure
        // (collision) doesn't block settlement.
        if expected_change > 0 {
            let now = now_ms();
            let change_utxo = InventoryUtxo {
                outpoint: Outpoint::new(witness_txid.clone(), 1),
                asset_id: quote.base_asset.clone(),
                amount: expected_change,
                btc_sats: 0,
                status: InventoryStatus::PendingBitcoinConfirm {
                    reservation_id: reservation_id.clone(),
                    witness_txid: witness_txid.clone(),
                },
                created_at_ms: now,
                updated_at_ms: now,
                pending_txid: Some(witness_txid.clone()),
            };
            if let Err(e) = self.store.ingest_change_utxo(change_utxo).await {
                eprintln!("change re-ingestion failed (continuing): {e}");
            }
        }

        Ok(SettlementIntent {
            quote_id: quote.quote_id,
            maker_id: self.maker_id.clone(),
            status: SettlementStatus::Ready,
            transfer: Some(transfer),
        })
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn synth_outpoint(idx: usize) -> Outpoint {
    Outpoint::new(format!("{idx:064x}"), 0)
}

fn legacy_state_to_inventory_status(idx: usize, state: AllocationState) -> InventoryStatus {
    match state {
        AllocationState::Available => InventoryStatus::Available,
        AllocationState::Reserved {
            quote_id,
            expires_at_ms,
        } => InventoryStatus::Reserved {
            reservation_id: ReservationId(format!("test-res-{idx}")),
            rfq_id: RfqId(format!("test-rfq-{idx}")),
            quote_id,
            expires_at_ms,
        },
        AllocationState::Spent { quote_id } => InventoryStatus::Spent {
            witness_txid: format!("test-wt-{idx}"),
            quote_id,
        },
    }
}

fn utxo_to_managed_allocation(utxo: InventoryUtxo, maker_id: MakerId) -> ManagedAllocation {
    let state = match utxo.status {
        InventoryStatus::Available => AllocationState::Available,
        InventoryStatus::Reserved {
            quote_id,
            expires_at_ms,
            ..
        } => AllocationState::Reserved {
            quote_id,
            expires_at_ms,
        },
        InventoryStatus::Spent { quote_id, .. } => AllocationState::Spent { quote_id },
        other => panic!(
            "inventory_snapshot() cannot represent {other:?}; \
             use utxo_snapshot() or extended_inventory_summary() instead"
        ),
    };
    ManagedAllocation {
        allocation: Allocation {
            maker_id,
            asset: utxo.asset_id,
            available_amount: utxo.amount,
        },
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_rgb::MockRgbBackend;
    use rfq_types::{AssetId, AssetKind, BitcoinNetwork, Side};

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

    fn allocation() -> Allocation {
        Allocation {
            maker_id: maker_id(),
            asset: asset(),
            available_amount: 100,
        }
    }

    fn allocation_with_amount(amount: u64) -> Allocation {
        Allocation {
            maker_id: maker_id(),
            asset: asset(),
            available_amount: amount,
        }
    }

    fn maker() -> MockMaker {
        let allocation = allocation();
        let rgb_backend = Arc::new(MockRgbBackend::new(vec![allocation.clone()]));
        MockMaker::new(maker_id(), vec![allocation], rgb_backend)
    }

    fn maker_with_inventory(inventory: Vec<ManagedAllocation>) -> MockMaker {
        let rgb_backend = Arc::new(MockRgbBackend::new(vec![allocation()]));
        MockMaker::new_with_inventory(maker_id(), inventory, rgb_backend)
    }

    fn maker_with_allocations(allocations: Vec<Allocation>) -> MockMaker {
        let rgb_backend = Arc::new(MockRgbBackend::new(allocations.clone()));
        MockMaker::new(maker_id(), allocations, rgb_backend)
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
        let inventory = maker.inventory_snapshot().await;
        assert_eq!(inventory.len(), 1);
        assert!(matches!(
            &inventory[0].state,
            AllocationState::Reserved { quote_id, expires_at_ms }
                if quote_id == &quote.quote_id && *expires_at_ms == quote.expires_at_ms
        ));
    }

    #[tokio::test]
    async fn inventory_summary_counts_initial_inventory_as_available() {
        let maker = maker_with_inventory(vec![
            ManagedAllocation {
                allocation: allocation_with_amount(100),
                state: AllocationState::Available,
            },
            ManagedAllocation {
                allocation: allocation_with_amount(200),
                state: AllocationState::Available,
            },
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
        let maker = maker_with_inventory(vec![ManagedAllocation {
            allocation: allocation(),
            state: AllocationState::Reserved {
                quote_id: QuoteId("expired-quote".to_owned()),
                expires_at_ms: 0,
            },
        }]);

        let snapshot = maker.inventory_summary().await;
        let inventory = maker.inventory_snapshot().await;

        assert_eq!(snapshot.available_amount, 100);
        assert_eq!(snapshot.reserved_amount, 0);
        assert_eq!(snapshot.available_allocations, 1);
        assert_eq!(snapshot.reserved_allocations, 0);
        assert!(matches!(inventory[0].state, AllocationState::Available));
    }

    #[tokio::test]
    async fn release_expired_reservations_returns_count_and_releases() {
        let maker = maker_with_inventory(vec![ManagedAllocation {
            allocation: allocation(),
            state: AllocationState::Reserved {
                quote_id: QuoteId("expired-quote".to_owned()),
                expires_at_ms: 0,
            },
        }]);

        let released = maker.release_expired_reservations().await;
        let inventory = maker.inventory_snapshot().await;

        assert_eq!(released, 1);
        assert!(matches!(inventory[0].state, AllocationState::Available));
    }

    #[tokio::test]
    async fn release_expired_reservations_ignores_active_and_spent() {
        let maker = maker_with_inventory(vec![
            ManagedAllocation {
                allocation: allocation(),
                state: AllocationState::Reserved {
                    quote_id: QuoteId("active-quote".to_owned()),
                    expires_at_ms: now_ms() + 30_000,
                },
            },
            ManagedAllocation {
                allocation: allocation(),
                state: AllocationState::Spent {
                    quote_id: QuoteId("spent-quote".to_owned()),
                },
            },
        ]);

        let released = maker.release_expired_reservations().await;
        let inventory = maker.inventory_snapshot().await;

        assert_eq!(released, 0);
        assert!(matches!(
            inventory[0].state,
            AllocationState::Reserved { .. }
        ));
        assert!(matches!(inventory[1].state, AllocationState::Spent { .. }));
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
        let maker = maker_with_inventory(vec![ManagedAllocation {
            allocation: allocation(),
            state: AllocationState::Reserved {
                quote_id: expired_quote_id,
                expires_at_ms: 0,
            },
        }]);

        let quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();

        assert!(quote.is_some());
        let inventory = maker.inventory_snapshot().await;
        assert!(matches!(
            &inventory[0].state,
            AllocationState::Reserved { quote_id, .. } if quote_id == &quote.unwrap().quote_id
        ));
    }

    #[tokio::test]
    async fn accept_marks_reserved_allocation_spent() {
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote should be returned");

        let intent = maker
            .accept_quote(
                quote.clone(),
                AcceptQuoteRequest {
                    quote_id: quote.quote_id.clone(),
                    rgb_invoice: "rgb:test_invoice".to_owned(),
                },
            )
            .await
            .unwrap();

        assert_eq!(intent.status, SettlementStatus::Ready);
        let inventory = maker.inventory_snapshot().await;
        assert!(matches!(
            &inventory[0].state,
            AllocationState::Spent { quote_id } if quote_id == &quote.quote_id
        ));
    }

    #[tokio::test]
    async fn inventory_summary_counts_spent_after_accept() {
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
                    quote_id: quote.quote_id,
                    rgb_invoice: "rgb:test_invoice".to_owned(),
                },
            )
            .await
            .unwrap();

        let snapshot = maker.inventory_summary().await;

        assert_eq!(snapshot.available_amount, 0);
        assert_eq!(snapshot.reserved_amount, 0);
        assert_eq!(snapshot.spent_amount, 100);
        assert_eq!(snapshot.available_allocations, 0);
        assert_eq!(snapshot.reserved_allocations, 0);
        assert_eq!(snapshot.spent_allocations, 1);
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
                    rgb_invoice: "rgb:test_invoice".to_owned(),
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
        let maker = maker_with_allocations(vec![
            allocation_with_amount(100),
            allocation_with_amount(100),
            allocation_with_amount(100),
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
        let maker = maker_with_inventory(vec![
            ManagedAllocation {
                allocation: allocation_with_amount(100),
                state: AllocationState::Reserved {
                    quote_id: QuoteId("expired".to_owned()),
                    expires_at_ms: 0,
                },
            },
            ManagedAllocation {
                allocation: allocation_with_amount(200),
                state: AllocationState::Available,
            },
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
        let maker = Arc::new(maker_with_allocations(vec![
            allocation_with_amount(100),
            allocation_with_amount(100),
            allocation_with_amount(100),
            allocation_with_amount(100),
            allocation_with_amount(100),
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
        let maker = maker_with_allocations(vec![allocation_with_amount(1000)]);
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
        let maker = maker_with_allocations(vec![
            allocation_with_amount(100),
            allocation_with_amount(100),
            allocation_with_amount(100),
            allocation_with_amount(100),
            allocation_with_amount(100),
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
        let allocations: Vec<_> = (0..10).map(|_| allocation_with_amount(10)).collect();
        let maker = maker_with_allocations(allocations);
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
        let maker = maker_with_allocations(vec![allocation_with_amount(1000)]);
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
    async fn accept_with_change_ingests_pending_bitcoin_confirm_utxo() {
        // Single allocation of 100; request 60. Selection picks the 100 UTXO,
        // change = 40. After accept_quote, inventory should show the input
        // UTXO as Spent and a synthetic change UTXO as PendingBitcoinConfirm.
        let allocation = allocation_with_amount(100);
        let rgb_backend = Arc::new(MockRgbBackend::new(vec![allocation.clone()]));
        let maker = MockMaker::new(maker_id(), vec![allocation], rgb_backend);

        let mut request = quote_request("rfq-1");
        request.amount = 60;
        let quote = maker
            .request_quote(request)
            .await
            .unwrap()
            .expect("quote should be issued");

        let intent = maker
            .accept_quote(
                quote.clone(),
                AcceptQuoteRequest {
                    quote_id: quote.quote_id.clone(),
                    rgb_invoice: "rgb:test".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(intent.status, SettlementStatus::Ready);

        let utxos = maker.utxo_snapshot().await;
        let spent_count = utxos
            .iter()
            .filter(|u| matches!(u.status, InventoryStatus::Spent { .. }))
            .count();
        let pending_change = utxos
            .iter()
            .find(|u| matches!(u.status, InventoryStatus::PendingBitcoinConfirm { .. }));
        assert_eq!(spent_count, 1);
        let pending_change = pending_change.expect("expected a change UTXO");
        assert_eq!(pending_change.amount, 40);
        assert!(pending_change.pending_txid.is_some());
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
                    rgb_invoice: "not-an-invoice".into(),
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
        let maker = maker_with_inventory(vec![
            ManagedAllocation {
                allocation: allocation_with_amount(100),
                state: AllocationState::Available,
            },
            ManagedAllocation {
                allocation: allocation_with_amount(200),
                state: AllocationState::Reserved {
                    quote_id: QuoteId("q".to_owned()),
                    expires_at_ms: now_ms() + 60_000,
                },
            },
        ]);

        let legacy = maker.inventory_summary().await;
        let ext = maker.extended_inventory_summary().await;
        let derived: InventorySnapshot = (&ext).into();
        assert_eq!(legacy, derived);
    }
}
