use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::Url;
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

pub struct HttpMakerConnector {
    maker_id: MakerId,
    base_url: Url,
    http: reqwest::Client,
}

impl HttpMakerConnector {
    pub fn new(maker_id: MakerId, base_url: Url) -> Self {
        Self {
            maker_id,
            base_url,
            http: reqwest::Client::new(),
        }
    }

    fn endpoint(&self, path: &str) -> Result<Url, RouterError> {
        self.base_url
            .join(path)
            .map_err(|error| RouterError::Maker(format!("invalid maker URL: {error}")))
    }
}

#[async_trait]
impl MakerConnector for HttpMakerConnector {
    fn maker_id(&self) -> MakerId {
        self.maker_id.clone()
    }

    async fn request_quote(&self, request: QuoteRequest) -> Result<Option<Quote>, RouterError> {
        let response = self
            .http
            .post(self.endpoint("quotes")?)
            .json(&request)
            .send()
            .await
            .map_err(|error| RouterError::Maker(error.to_string()))?;

        parse_maker_response(response).await
    }

    async fn accept_quote(
        &self,
        quote: Quote,
        request: AcceptQuoteRequest,
    ) -> Result<SettlementIntent, RouterError> {
        let response = self
            .http
            .post(self.endpoint(&format!("quotes/{}/accept", quote.quote_id.0))?)
            .json(&request)
            .send()
            .await
            .map_err(|error| RouterError::Maker(error.to_string()))?;

        parse_maker_response(response).await
    }
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

async fn parse_maker_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, RouterError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(RouterError::Maker(format!(
            "maker returned {status}: {body}"
        )));
    }

    response
        .json::<T>()
        .await
        .map_err(|error| RouterError::Maker(error.to_string()))
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

    #[test]
    fn http_maker_connector_builds_protocol_urls() {
        let connector = HttpMakerConnector::new(
            MakerId("maker-1".to_owned()),
            Url::parse("http://127.0.0.1:4000/").unwrap(),
        );

        assert_eq!(
            connector.endpoint("quotes").unwrap().as_str(),
            "http://127.0.0.1:4000/quotes"
        );
        assert_eq!(
            connector
                .endpoint("quotes/quote-1/accept")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:4000/quotes/quote-1/accept"
        );
    }
}
