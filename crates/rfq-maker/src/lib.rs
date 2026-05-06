use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use rfq_rgb::RgbBackend;
use rfq_router::{MakerConnector, RouterError};
use rfq_types::{
    AcceptQuoteRequest, Allocation, AllocationState, InventorySnapshot, MakerId, ManagedAllocation,
    Quote, QuoteId, QuoteRequest, SettlementIntent, SettlementStatus,
};
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Clone)]
pub struct MockMaker {
    maker_id: MakerId,
    inventory: Arc<RwLock<Vec<ManagedAllocation>>>,
    rgb_backend: Arc<dyn RgbBackend>,
}

impl MockMaker {
    pub fn new(
        maker_id: MakerId,
        allocations: Vec<Allocation>,
        rgb_backend: Arc<dyn RgbBackend>,
    ) -> Self {
        let inventory = allocations
            .into_iter()
            .map(|allocation| ManagedAllocation {
                allocation,
                state: AllocationState::Available,
            })
            .collect();

        Self::new_with_inventory(maker_id, inventory, rgb_backend)
    }

    pub fn new_with_inventory(
        maker_id: MakerId,
        inventory: Vec<ManagedAllocation>,
        rgb_backend: Arc<dyn RgbBackend>,
    ) -> Self {
        Self {
            maker_id,
            inventory: Arc::new(RwLock::new(inventory)),
            rgb_backend,
        }
    }

    pub async fn inventory_snapshot(&self) -> Vec<ManagedAllocation> {
        self.inventory.read().await.clone()
    }

    pub async fn inventory_summary(&self) -> InventorySnapshot {
        let mut inventory = self.inventory.write().await;
        release_expired_reservations(&mut inventory, now_ms());

        summarize_inventory(&inventory)
    }
}

#[async_trait]
impl MakerConnector for MockMaker {
    fn maker_id(&self) -> MakerId {
        self.maker_id.clone()
    }

    async fn request_quote(&self, request: QuoteRequest) -> Result<Option<Quote>, RouterError> {
        let mut inventory = self.inventory.write().await;
        let now_ms = now_ms();
        release_expired_reservations(&mut inventory, now_ms);

        let Some(allocation) = inventory.iter_mut().find(|managed| {
            matches!(&managed.state, AllocationState::Available)
                && managed.allocation.asset == request.base_asset
                && managed.allocation.available_amount >= request.amount
        }) else {
            return Ok(None);
        };

        let quote_id = QuoteId(Uuid::new_v4().to_string());
        let expires_at_ms = now_ms + Duration::from_secs(30).as_millis() as u64;
        allocation.state = AllocationState::Reserved {
            quote_id: quote_id.clone(),
            expires_at_ms,
        };

        Ok(Some(Quote {
            quote_id,
            rfq_id: request.rfq_id,
            maker_id: self.maker_id.clone(),
            base_asset: request.base_asset,
            quote_asset: request.quote_asset,
            side: request.side,
            amount: request.amount,
            price: request.amount.saturating_mul(101),
            expires_at_ms,
        }))
    }

    async fn accept_quote(
        &self,
        quote: Quote,
        request: AcceptQuoteRequest,
    ) -> Result<SettlementIntent, RouterError> {
        {
            let mut inventory = self.inventory.write().await;
            release_expired_reservations(&mut inventory, now_ms());

            if !has_reserved_quote(&inventory, &quote.quote_id) {
                return Err(RouterError::Maker(
                    "quote reservation not found or expired".to_owned(),
                ));
            }
        }

        let transfer = self
            .rgb_backend
            .create_transfer(&request.rgb_invoice, quote.amount)
            .await
            .map_err(|error| RouterError::Maker(error.to_string()))?;

        {
            let mut inventory = self.inventory.write().await;
            let Some(allocation) = inventory.iter_mut().find(|managed| {
                matches!(
                    &managed.state,
                    AllocationState::Reserved { quote_id, .. } if quote_id == &quote.quote_id
                )
            }) else {
                return Err(RouterError::Maker(
                    "quote reservation not found or expired".to_owned(),
                ));
            };

            allocation.state = AllocationState::Spent {
                quote_id: quote.quote_id.clone(),
            };
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

fn release_expired_reservations(inventory: &mut [ManagedAllocation], now_ms: u64) {
    for allocation in inventory {
        if matches!(
            &allocation.state,
            AllocationState::Reserved { expires_at_ms, .. } if *expires_at_ms <= now_ms
        ) {
            allocation.state = AllocationState::Available;
        }
    }
}

fn has_reserved_quote(inventory: &[ManagedAllocation], quote_id: &QuoteId) -> bool {
    inventory.iter().any(|managed| {
        matches!(
            &managed.state,
            AllocationState::Reserved { quote_id: reserved_quote_id, .. }
                if reserved_quote_id == quote_id
        )
    })
}

fn summarize_inventory(inventory: &[ManagedAllocation]) -> InventorySnapshot {
    let mut snapshot = InventorySnapshot::default();

    for allocation in inventory {
        let amount = allocation.allocation.available_amount;
        snapshot.total_amount = snapshot.total_amount.saturating_add(amount);
        snapshot.total_allocations += 1;

        match allocation.state {
            AllocationState::Available => {
                snapshot.available_amount = snapshot.available_amount.saturating_add(amount);
                snapshot.available_allocations += 1;
            }
            AllocationState::Reserved { .. } => {
                snapshot.reserved_amount = snapshot.reserved_amount.saturating_add(amount);
                snapshot.reserved_allocations += 1;
            }
            AllocationState::Spent { .. } => {
                snapshot.spent_amount = snapshot.spent_amount.saturating_add(amount);
                snapshot.spent_allocations += 1;
            }
        }
    }

    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_rgb::MockRgbBackend;
    use rfq_types::{AssetId, AssetKind, BitcoinNetwork, RfqId, Side};

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
}
