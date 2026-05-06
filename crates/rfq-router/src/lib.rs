use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use rfq_core::{is_quote_expired, sort_quotes_best_price, validate_quote_request, RfqCoreError};
use rfq_types::{AcceptQuoteRequest, MakerId, Quote, QuoteRequest, SettlementIntent};
use thiserror::Error;
use tokio::time::timeout;

const MAKER_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Error)]
pub enum RouterError {
    #[error("invalid quote request: {0}")]
    InvalidRequest(#[from] RfqCoreError),
    #[error("maker error: {0}")]
    Maker(String),
}

#[async_trait]
pub trait MakerConnector: Send + Sync {
    fn maker_id(&self) -> MakerId;

    async fn request_quote(&self, request: QuoteRequest) -> Result<Option<Quote>, RouterError>;

    async fn accept_quote(
        &self,
        quote: Quote,
        request: AcceptQuoteRequest,
    ) -> Result<SettlementIntent, RouterError>;
}

pub async fn fanout_quote(
    makers: &[Arc<dyn MakerConnector>],
    request: QuoteRequest,
) -> Result<Vec<Quote>, RouterError> {
    validate_quote_request(&request)?;

    let mut futures = FuturesUnordered::new();
    for maker in makers {
        let maker = Arc::clone(maker);
        let request = request.clone();
        futures.push(async move { timeout(MAKER_TIMEOUT, maker.request_quote(request)).await });
    }

    let now_ms = now_ms();
    let mut quotes = Vec::new();
    while let Some(result) = futures.next().await {
        if let Ok(Ok(Some(quote))) = result {
            if quote.rfq_id == request.rfq_id
                && quote.amount == request.amount
                && quote.base_asset == request.base_asset
                && quote.quote_asset == request.quote_asset
                && quote.side == request.side
                && !is_quote_expired(&quote, now_ms)
            {
                quotes.push(quote);
            }
        }
    }

    sort_quotes_best_price(&mut quotes);
    Ok(quotes)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
