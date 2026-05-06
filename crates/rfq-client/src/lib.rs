use reqwest::StatusCode;
pub use reqwest::Url;
use rfq_types::{AcceptQuoteRequest, CreateRfqRequest, HealthResponse, Quote, SettlementIntent};
use thiserror::Error;

pub struct RfqClient {
    base_url: Url,
    http: reqwest::Client,
}

impl RfqClient {
    pub fn new(base_url: Url) -> Self {
        Self {
            base_url,
            http: reqwest::Client::new(),
        }
    }

    pub async fn request_quotes(
        &self,
        request: CreateRfqRequest,
    ) -> Result<Vec<Quote>, RfqClientError> {
        let url = self.endpoint("rfq")?;
        let response = self.http.post(url).json(&request).send().await?;

        parse_response(response).await
    }

    pub async fn health(&self) -> Result<HealthResponse, RfqClientError> {
        let url = self.endpoint("health")?;
        let response = self.http.get(url).send().await?;

        parse_response(response).await
    }

    pub async fn accept_quote(
        &self,
        request: AcceptQuoteRequest,
    ) -> Result<SettlementIntent, RfqClientError> {
        let url = self.endpoint(&format!("quotes/{}/accept", request.quote_id.0))?;
        let response = self.http.post(url).json(&request).send().await?;

        parse_response(response).await
    }

    fn endpoint(&self, path: &str) -> Result<Url, RfqClientError> {
        self.base_url
            .join(path)
            .map_err(|error| RfqClientError::InvalidUrl(error.to_string()))
    }
}

async fn parse_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, RfqClientError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(RfqClientError::HttpStatus { status, body });
    }

    Ok(response.json::<T>().await?)
}

#[derive(Debug, Error)]
pub enum RfqClientError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("API returned {status}: {body}")]
    HttpStatus { status: StatusCode, body: String },
}
