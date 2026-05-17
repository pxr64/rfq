use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use rfq_api::{AcceptQuoteBody, CreateRfqRequest};
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
            btc_funding_addr: "bcrt1qtaker".to_owned(),
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
