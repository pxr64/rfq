use reqwest::StatusCode;
pub use reqwest::Url;
use rfq_types::{
    AcceptQuoteRequest, CreateRfqRequest, HealthResponse, Outpoint, Quote, QuoteId,
    SettlementIntent,
};
use serde::Serialize;
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

    /// Sell-side: submit the taker's **provenance** consignment plus the explicit
    /// list of RGB outpoints being sold. The maker validates the consignment,
    /// confirms the RGB at those outpoints, builds the swap PSBT, and returns a
    /// `SettlementIntent` in `AwaitingTakerSignature`. See
    /// `docs/provenance-consignment-proposal.md`.
    pub async fn submit_consignment(
        &self,
        quote_id: QuoteId,
        consignment_base64: String,
        outpoints: Vec<Outpoint>,
    ) -> Result<SettlementIntent, RfqClientError> {
        let url = self.endpoint(&format!("quotes/{}/consignment", quote_id.0))?;
        let response = self
            .http
            .post(url)
            .json(&ConsignmentRequest {
                consignment: consignment_base64,
                outpoints,
            })
            .send()
            .await?;

        parse_response(response).await
    }

    /// Both sides: submit the fully-signed PSBT. The maker finalizes the
    /// witness tx, broadcasts it, and returns a `SettlementIntent` in
    /// `PendingBitcoinConfirm`.
    pub async fn submit_signed_psbt(
        &self,
        quote_id: QuoteId,
        signed_psbt_base64: String,
    ) -> Result<SettlementIntent, RfqClientError> {
        let url = self.endpoint(&format!("quotes/{}/sign", quote_id.0))?;
        let response = self
            .http
            .post(url)
            .json(&SignedPsbtRequest {
                signed_psbt: signed_psbt_base64,
            })
            .send()
            .await?;

        parse_response(response).await
    }

    fn endpoint(&self, path: &str) -> Result<Url, RfqClientError> {
        self.base_url
            .join(path)
            .map_err(|error| RfqClientError::InvalidUrl(error.to_string()))
    }
}

#[derive(Serialize)]
struct ConsignmentRequest {
    consignment: String,
    outpoints: Vec<Outpoint>,
}

#[derive(Serialize)]
struct SignedPsbtRequest {
    signed_psbt: String,
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
