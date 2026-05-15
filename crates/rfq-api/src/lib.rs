use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rfq_btc::MockBitcoinClient;
use rfq_core::is_quote_expired;
use rfq_maker::MockMaker;
use rfq_rgb::MockRgbBackend;
use rfq_router::{fanout_quote, MakerConnector};
use rfq_store::{InMemoryQuoteStore, QuoteStore};
pub use rfq_types::CreateRfqRequest;
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, HealthResponse, MakerId, Outpoint,
    Quote, QuoteId, QuoteRequest, RfqId, RgbInventoryUtxo, SettlementIntent, SwapLeg,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    makers: Vec<Arc<dyn MakerConnector>>,
    store: InMemoryQuoteStore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptQuoteBody {
    pub leg: SwapLeg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignQuoteBody {
    /// Base64 PSBT, taker-signed. See `docs/swap-flows.md`.
    pub signed_psbt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsignmentBody {
    /// Base64 RGB consignment the taker built against the maker's
    /// `Quote.maker_rgb_invoice` (sell side). See `docs/swap-flows.md`.
    pub consignment: String,
}

pub fn app() -> Router {
    let maker_id = MakerId("mock-maker-1".to_owned());
    let rgb_asset = AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Rgb20,
        id: "rgb-test-asset".to_owned(),
    };
    let utxo = RgbInventoryUtxo {
        outpoint: Outpoint::new(format!("{:064x}", 0u64), 0),
        asset_id: rgb_asset,
        amount: 1_000_000,
        btc_sats: 0,
    };
    let rgb_backend = Arc::new(MockRgbBackend::new(vec![utxo.clone()]));
    let bitcoin_client = Arc::new(MockBitcoinClient::new());
    let maker = Arc::new(MockMaker::new(maker_id, vec![utxo], rgb_backend, bitcoin_client));

    app_with_state(AppState {
        makers: vec![maker],
        store: InMemoryQuoteStore::new(),
    })
}

pub fn app_with_state(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/rfq", post(create_rfq))
        .route("/quotes/:id/accept", post(accept_quote))
        .route("/quotes/:id/consignment", post(deliver_consignment))
        .route("/quotes/:id/sign", post(sign_quote))
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_owned(),
    })
}

async fn create_rfq(
    State(state): State<AppState>,
    Json(body): Json<CreateRfqRequest>,
) -> Result<Json<Vec<Quote>>, ApiError> {
    let request = QuoteRequest {
        rfq_id: RfqId(Uuid::new_v4().to_string()),
        base_asset: body.base_asset,
        quote_asset: body.quote_asset,
        side: body.side,
        amount: body.amount,
        created_at_ms: now_ms(),
    };
    let quotes = fanout_quote(&state.makers, request).await?;

    for quote in &quotes {
        state.store.save_quote(quote.clone()).await;
    }

    Ok(Json(quotes))
}

async fn accept_quote(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AcceptQuoteBody>,
) -> Result<Json<SettlementIntent>, ApiError> {
    let quote_id = QuoteId(id);
    let quote = state
        .store
        .get_quote(&quote_id)
        .await
        .ok_or(ApiError::NotFound)?;

    if is_quote_expired(&quote, now_ms()) {
        return Err(ApiError::QuoteExpired);
    }

    let maker = state
        .makers
        .iter()
        .find(|maker| maker.maker_id() == quote.maker_id)
        .ok_or(ApiError::MakerNotFound)?;

    let intent = maker
        .accept_quote(
            quote,
            AcceptQuoteRequest {
                quote_id,
                leg: body.leg,
            },
        )
        .await?;

    Ok(Json(intent))
}

async fn deliver_consignment(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ConsignmentBody>,
) -> Result<Json<SettlementIntent>, ApiError> {
    let quote_id = QuoteId(id);
    let quote = state
        .store
        .get_quote(&quote_id)
        .await
        .ok_or(ApiError::NotFound)?;

    // Sell-side `/consignment` runs before `accept`, so the quote TTL still
    // governs here — mirror `accept_quote`. 16c moves this onto the settlement
    // stage TTL once accept(Sell) is wired.
    if is_quote_expired(&quote, now_ms()) {
        return Err(ApiError::QuoteExpired);
    }

    let maker = state
        .makers
        .iter()
        .find(|maker| maker.maker_id() == quote.maker_id)
        .ok_or(ApiError::MakerNotFound)?;

    let intent = maker
        .deliver_consignment(quote_id, body.consignment)
        .await?;

    Ok(Json(intent))
}

async fn sign_quote(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SignQuoteBody>,
) -> Result<Json<SettlementIntent>, ApiError> {
    let quote_id = QuoteId(id);
    // No `is_quote_expired` check here: past accept the settlement runs on its
    // own TTL window, which `submit_signed_psbt` enforces internally.
    let quote = state
        .store
        .get_quote(&quote_id)
        .await
        .ok_or(ApiError::NotFound)?;

    let maker = state
        .makers
        .iter()
        .find(|maker| maker.maker_id() == quote.maker_id)
        .ok_or(ApiError::MakerNotFound)?;

    let intent = maker.submit_signed_psbt(quote_id, body.signed_psbt).await?;

    Ok(Json(intent))
}

#[derive(Debug)]
enum ApiError {
    BadRequest(String),
    NotFound,
    QuoteExpired,
    MakerNotFound,
    ConsignmentRejected(String),
    PsbtInvalid(String),
    FeeSlippageExceeded { estimated: u64, actual: u64 },
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "quote not found".to_owned()),
            ApiError::QuoteExpired => (StatusCode::BAD_REQUEST, "quote expired".to_owned()),
            ApiError::MakerNotFound => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "maker not found".to_owned(),
            ),
            ApiError::ConsignmentRejected(message) => (
                StatusCode::BAD_REQUEST,
                format!("consignment rejected: {message}"),
            ),
            ApiError::PsbtInvalid(message) => (
                StatusCode::BAD_REQUEST,
                format!("psbt invalid: {message}"),
            ),
            ApiError::FeeSlippageExceeded { estimated, actual } => (
                StatusCode::BAD_REQUEST,
                format!("fee slippage exceeded: estimated {estimated} sats, actual {actual} sats"),
            ),
        };

        (status, Json(ErrorResponse { error: message })).into_response()
    }
}

impl From<rfq_router::RouterError> for ApiError {
    fn from(error: rfq_router::RouterError) -> Self {
        match error {
            rfq_router::RouterError::InvalidRequest(error) => {
                ApiError::BadRequest(error.to_string())
            }
            rfq_router::RouterError::Maker(error) => ApiError::BadRequest(error),
            rfq_router::RouterError::FeeSlippageExceeded { estimated, actual } => {
                ApiError::FeeSlippageExceeded { estimated, actual }
            }
            rfq_router::RouterError::ConsignmentRejected(msg) => {
                ApiError::ConsignmentRejected(msg)
            }
            rfq_router::RouterError::PsbtInvalid(msg) => ApiError::PsbtInvalid(msg),
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
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
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use rfq_store::QuoteStore;
    use rfq_types::Side;
    use tower::ServiceExt;

    fn test_maker() -> Arc<dyn MakerConnector> {
        let rgb_asset = AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-test-asset".to_owned(),
        };
        let utxo = RgbInventoryUtxo {
            outpoint: Outpoint::new(format!("{:064x}", 0u64), 0),
            asset_id: rgb_asset,
            amount: 1_000_000,
            btc_sats: 0,
        };
        let rgb_backend = Arc::new(MockRgbBackend::new(vec![utxo.clone()]));
        let bitcoin_client = Arc::new(MockBitcoinClient::new());
        Arc::new(MockMaker::new(
            MakerId("mock-maker-1".to_owned()),
            vec![utxo],
            rgb_backend,
            bitcoin_client,
        ))
    }

    // `/consignment` runs before `accept`, so an elapsed quote TTL still aborts
    // the call — the API layer rejects it before reaching the maker.
    #[tokio::test]
    async fn consignment_for_expired_quote_returns_400() {
        let maker = test_maker();
        let store = InMemoryQuoteStore::new();

        let asset = |kind, id: &str| AssetId {
            network: BitcoinNetwork::Regtest,
            kind,
            id: id.to_owned(),
        };
        store
            .save_quote(Quote {
                quote_id: QuoteId("expired-quote".to_owned()),
                rfq_id: RfqId("rfq-x".to_owned()),
                maker_id: maker.maker_id(),
                base_asset: asset(AssetKind::Rgb20, "rgb-test-asset"),
                quote_asset: asset(AssetKind::Btc, "btc"),
                side: Side::Sell,
                amount: 100,
                price: 10_100,
                // TTL lapsed an hour ago.
                expires_at_ms: now_ms() - 3_600_000,
                estimated_fee_sats: 0,
                fee_slippage_bps: 2000,
                maker_rgb_invoice: Some("rgb:mock-maker-invoice-expired-quote".to_owned()),
            })
            .await;

        let app = app_with_state(AppState {
            makers: vec![maker],
            store,
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/quotes/expired-quote/consignment")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&ConsignmentBody {
                            consignment: "mock-consignment:sell:amount=100".to_owned(),
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
