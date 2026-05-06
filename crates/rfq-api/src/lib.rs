use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rfq_core::is_quote_expired;
use rfq_maker::MockMaker;
use rfq_rgb::MockRgbBackend;
use rfq_router::{fanout_quote, MakerConnector};
use rfq_store::{InMemoryQuoteStore, QuoteStore};
pub use rfq_types::CreateRfqRequest;
use rfq_types::{
    AcceptQuoteRequest, Allocation, AssetId, AssetKind, BitcoinNetwork, HealthResponse, MakerId,
    Quote, QuoteId, QuoteRequest, RfqId, SettlementIntent,
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
    pub rgb_invoice: String,
}

pub fn app() -> Router {
    let maker_id = MakerId("mock-maker-1".to_owned());
    let rgb_asset = AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Rgb20,
        id: "rgb-test-asset".to_owned(),
    };
    let allocation = Allocation {
        maker_id: maker_id.clone(),
        asset: rgb_asset,
        available_amount: 1_000_000,
    };
    let rgb_backend = Arc::new(MockRgbBackend::new(vec![allocation.clone()]));
    let maker = Arc::new(MockMaker::new(maker_id, vec![allocation], rgb_backend));

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
                rgb_invoice: body.rgb_invoice,
            },
        )
        .await?;

    Ok(Json(intent))
}

#[derive(Debug)]
enum ApiError {
    BadRequest(String),
    NotFound,
    QuoteExpired,
    MakerNotFound,
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
