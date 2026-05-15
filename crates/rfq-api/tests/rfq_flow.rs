use axum::{
    body::{to_bytes, Body},
    http::{Request, Response, StatusCode},
};
use rfq_api::{AcceptQuoteBody, ConsignmentBody, CreateRfqRequest};
use rfq_types::{
    AssetId, AssetKind, BitcoinNetwork, HealthResponse, Quote, SettlementIntent, SettlementStatus,
    Side, SwapLeg,
};
use tower::ServiceExt;

#[tokio::test]
async fn health_returns_ok() {
    let app = rfq_api::app();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let health: HealthResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(health.status, "ok");
}

#[tokio::test]
async fn rfq_quote_accept_flow_succeeds() {
    let app = rfq_api::app();

    let rfq_body = CreateRfqRequest {
        base_asset: AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-test-asset".to_owned(),
        },
        quote_asset: AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Btc,
            id: "btc".to_owned(),
        },
        side: Side::Buy,
        amount: 100,
    };

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rfq")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&rfq_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let quotes: Vec<Quote> = serde_json::from_slice(&body).unwrap();
    assert_eq!(quotes.len(), 1);

    let quote_id = quotes[0].quote_id.0.clone();
    let accept_body = AcceptQuoteBody {
        leg: SwapLeg::Buy {
            rgb_invoice: "rgb:test_invoice".to_owned(),
        },
    };

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/quotes/{quote_id}/accept"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&accept_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let intent: SettlementIntent = serde_json::from_slice(&body).unwrap();
    assert_eq!(intent.status, SettlementStatus::AwaitingTakerSignature);
    assert!(intent.transfer.is_some());
}

/// Post a sell-side RFQ and return the resulting quote id.
async fn create_sell_quote(app: &axum::Router) -> String {
    let rfq_body = CreateRfqRequest {
        base_asset: AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-test-asset".to_owned(),
        },
        quote_asset: AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Btc,
            id: "btc".to_owned(),
        },
        side: Side::Sell,
        amount: 100,
    };

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rfq")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&rfq_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let quotes: Vec<Quote> = serde_json::from_slice(&body).unwrap();
    assert_eq!(quotes.len(), 1);
    // Sell-side quotes carry the maker's RGB invoice for the taker to consign to.
    assert!(quotes[0].maker_rgb_invoice.is_some());
    quotes[0].quote_id.0.clone()
}

async fn post_consignment(app: &axum::Router, quote_id: &str, consignment: &str) -> Response<Body> {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/quotes/{quote_id}/consignment"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&ConsignmentBody {
                        consignment: consignment.to_owned(),
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn consignment_for_unknown_quote_returns_404() {
    let app = rfq_api::app();

    let response = post_consignment(&app, "no-such-quote", "mock-consignment:sell").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn consignment_malformed_is_rejected() {
    let app = rfq_api::app();
    let quote_id = create_sell_quote(&app).await;

    let response = post_consignment(&app, &quote_id, "not-a-real-consignment").await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(error["error"]
        .as_str()
        .unwrap()
        .starts_with("consignment rejected:"));
}

#[tokio::test]
async fn sell_side_consignment_flow_succeeds() {
    let app = rfq_api::app();
    let quote_id = create_sell_quote(&app).await;

    let response = post_consignment(&app, &quote_id, "mock-consignment:sell:amount=100").await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let intent: SettlementIntent = serde_json::from_slice(&body).unwrap();
    assert_eq!(intent.status, SettlementStatus::AwaitingTakerSignature);
    let transfer = intent.transfer.expect("transfer");
    assert!(!transfer.partial_psbt.is_empty());
    assert!(transfer.expected_witness_txid.is_some());
}
