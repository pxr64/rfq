use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use rfq_rgb::RgbBackend;
use rfq_router::{MakerConnector, RouterError};
use rfq_types::{
    AcceptQuoteRequest, Allocation, MakerId, Quote, QuoteId, QuoteRequest, SettlementIntent,
    SettlementStatus,
};
use uuid::Uuid;

#[derive(Clone)]
pub struct MockMaker {
    maker_id: MakerId,
    allocations: Vec<Allocation>,
    rgb_backend: Arc<dyn RgbBackend>,
}

impl MockMaker {
    pub fn new(
        maker_id: MakerId,
        allocations: Vec<Allocation>,
        rgb_backend: Arc<dyn RgbBackend>,
    ) -> Self {
        Self {
            maker_id,
            allocations,
            rgb_backend,
        }
    }

    fn has_liquidity(&self, request: &QuoteRequest) -> bool {
        self.allocations.iter().any(|allocation| {
            allocation.asset == request.base_asset && allocation.available_amount >= request.amount
        })
    }
}

#[async_trait]
impl MakerConnector for MockMaker {
    fn maker_id(&self) -> MakerId {
        self.maker_id.clone()
    }

    async fn request_quote(&self, request: QuoteRequest) -> Result<Option<Quote>, RouterError> {
        if !self.has_liquidity(&request) {
            return Ok(None);
        }

        Ok(Some(Quote {
            quote_id: QuoteId(Uuid::new_v4().to_string()),
            rfq_id: request.rfq_id,
            maker_id: self.maker_id.clone(),
            base_asset: request.base_asset,
            quote_asset: request.quote_asset,
            side: request.side,
            amount: request.amount,
            price: request.amount.saturating_mul(101),
            expires_at_ms: now_ms() + Duration::from_secs(30).as_millis() as u64,
        }))
    }

    async fn accept_quote(
        &self,
        quote: Quote,
        request: AcceptQuoteRequest,
    ) -> Result<SettlementIntent, RouterError> {
        let transfer = self
            .rgb_backend
            .create_transfer(&request.rgb_invoice, quote.amount)
            .await
            .map_err(|error| RouterError::Maker(error.to_string()))?;

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
