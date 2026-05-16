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
use rfq_maker::Maker;
use rfq_rgb::MockRgbBackend;
use rfq_router::{fanout_quote, MakerConnector};
use rfq_store::{InMemoryQuoteStore, QuoteStore};
pub use rfq_types::CreateRfqRequest;
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, BtcInventoryStatus, BtcInventoryUtxo,
    HealthResponse, MakerId, Outpoint, Quote, QuoteId, QuoteRequest, RfqId, RgbInventoryUtxo,
    SettlementIntent, SwapLeg,
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
    // Seed BTC inventory so the maker can also quote the sell side.
    let maker = Arc::new(
        Maker::new(maker_id, vec![utxo], rgb_backend, bitcoin_client)
            .with_btc_inventory(mock_btc_inventory()),
    );

    app_with_state(AppState {
        makers: vec![maker],
        store: InMemoryQuoteStore::new(),
    })
}

/// A small pool of segwit BTC UTXOs for the maker to pay sell-side takers from.
fn mock_btc_inventory() -> Vec<BtcInventoryUtxo> {
    // P2WPKH script (`OP_0 <20 bytes>`) — the swap PSBT is segwit-only.
    let p2wpkh = || {
        let mut s = vec![0x00, 0x14];
        s.extend(std::iter::repeat_n(0x11, 20));
        s
    };
    (0..3u64)
        .map(|i| BtcInventoryUtxo {
            outpoint: Outpoint::new(format!("{:064x}", 0xb7c0 + i), 0),
            value_sats: 1_000_000,
            script_pubkey: p2wpkh(),
            status: BtcInventoryStatus::Available,
            created_at_ms: 0,
            updated_at_ms: 0,
            pending_txid: None,
        })
        .collect()
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

    // No `is_quote_expired` check: `/consignment` runs after `accept`, so the
    // settlement runs on its own stage TTL (the maker's BTC reservation
    // deadline), which `deliver_consignment` enforces internally — same as
    // `/sign`.
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
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use rfq_types::{SettlementStatus, Side};
    use tower::ServiceExt;

    fn rgb_asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-test-asset".to_owned(),
        }
    }

    fn btc_asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Btc,
            id: "btc".to_owned(),
        }
    }

    fn p2wpkh() -> Vec<u8> {
        let mut s = vec![0x00, 0x14];
        s.extend(std::iter::repeat_n(0x44, 20));
        s
    }

    /// One of the taker's RGB-bearing outpoints — seeded into the maker's
    /// bitcoin client below so `deliver_consignment` can resolve its prevout.
    fn taker_op(idx: u64) -> Outpoint {
        Outpoint::new(format!("{:064x}", 0xda7a + idx), 0)
    }

    /// A sell-capable maker: BTC inventory to pay out from, plus a bitcoin
    /// client that knows the taker's RGB outpoints.
    fn sell_app() -> Router {
        let rgb_backend = Arc::new(MockRgbBackend::new(vec![]));
        let mut client = MockBitcoinClient::new();
        for i in 0..3 {
            client = client.with_prevout(
                taker_op(i),
                rfq_btc::TxOut {
                    value_sats: 50_000,
                    script_pubkey: p2wpkh(),
                },
            );
        }
        let maker = Arc::new(
            Maker::new(
                MakerId("mock-maker-1".to_owned()),
                vec![],
                rgb_backend,
                Arc::new(client),
            )
            .with_btc_inventory(mock_btc_inventory()),
        );
        app_with_state(AppState {
            makers: vec![maker],
            store: InMemoryQuoteStore::new(),
        })
    }

    async fn send<T: Serialize>(
        app: &Router,
        uri: &str,
        body: &T,
    ) -> (StatusCode, Vec<u8>) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, bytes.to_vec())
    }

    fn sell_rfq() -> CreateRfqRequest {
        CreateRfqRequest {
            base_asset: rgb_asset(),
            quote_asset: btc_asset(),
            side: Side::Sell,
            amount: 100,
        }
    }

    #[tokio::test]
    async fn consignment_for_unknown_quote_returns_404() {
        let app = sell_app();
        let (status, _) = send(
            &app,
            "/quotes/no-such-quote/consignment",
            &ConsignmentBody {
                consignment: "mock-consignment|sell|".to_owned(),
            },
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn consignment_malformed_is_rejected() {
        let app = sell_app();

        let (_, body) = send(&app, "/rfq", &sell_rfq()).await;
        let quotes: Vec<Quote> = serde_json::from_slice(&body).unwrap();
        let quote = &quotes[0];

        // Accept the quote so the settlement reaches `AwaitingConsignment`.
        send(
            &app,
            &format!("/quotes/{}/accept", quote.quote_id.0),
            &AcceptQuoteBody {
                leg: SwapLeg::Sell {
                    btc_payout_addr: "bcrt1qtaker".to_owned(),
                    rgb_change_invoice: None,
                },
            },
        )
        .await;

        let (status, body) = send(
            &app,
            &format!("/quotes/{}/consignment", quote.quote_id.0),
            &ConsignmentBody {
                consignment: "not-a-real-consignment".to_owned(),
            },
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(error["error"]
            .as_str()
            .unwrap()
            .starts_with("consignment rejected:"));
    }

    /// Full sell-side HTTP round trip: rfq → accept → consignment → sign.
    #[tokio::test]
    async fn sell_swap_settles_end_to_end() {
        let app = sell_app();

        let (status, body) = send(&app, "/rfq", &sell_rfq()).await;
        assert_eq!(status, StatusCode::OK);
        let quotes: Vec<Quote> = serde_json::from_slice(&body).unwrap();
        let quote = &quotes[0];
        let invoice = quote.maker_rgb_invoice.clone().expect("sell quote invoice");

        let (status, body) = send(
            &app,
            &format!("/quotes/{}/accept", quote.quote_id.0),
            &AcceptQuoteBody {
                leg: SwapLeg::Sell {
                    btc_payout_addr: "bcrt1qtaker".to_owned(),
                    rgb_change_invoice: None,
                },
            },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let accepted: SettlementIntent = serde_json::from_slice(&body).unwrap();
        assert_eq!(accepted.status, SettlementStatus::AwaitingConsignment);

        let consignment = format!(
            "mock-consignment|sell|invoice={invoice}|amount={}|outpoints={},{}",
            quote.amount,
            taker_op(0),
            taker_op(1),
        );
        let (status, body) = send(
            &app,
            &format!("/quotes/{}/consignment", quote.quote_id.0),
            &ConsignmentBody { consignment },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let delivered: SettlementIntent = serde_json::from_slice(&body).unwrap();
        assert_eq!(delivered.status, SettlementStatus::AwaitingTakerSignature);
        let psbt = delivered.transfer.expect("transfer").partial_psbt;

        let (status, body) = send(
            &app,
            &format!("/quotes/{}/sign", quote.quote_id.0),
            &SignQuoteBody {
                signed_psbt: format!("{psbt}|signed"),
            },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let settled: SettlementIntent = serde_json::from_slice(&body).unwrap();
        assert_eq!(settled.status, SettlementStatus::PendingBitcoinConfirm);
        assert!(settled.witness_txid.is_some());
        assert!(settled.final_consignment.is_some());
    }
}
