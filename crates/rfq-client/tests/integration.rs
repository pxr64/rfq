//! End-to-end tests for the `RfqClient` SDK against a live `rfq-api` server.
//!
//! Everything goes over real HTTP (reqwest → a bound TCP port). The tests
//! touch only the public `rfq_api::app()` entry point and the `RfqClient`
//! surface — no rfq-api internals (`AppState`, handlers) — so they exercise
//! the SDK exactly as an external integrator would.

use rfq_client::{RfqClient, RfqClientError, Url};
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, CreateRfqRequest, QuoteId,
    SettlementStatus, Side, SwapLeg,
};

/// Bind an ephemeral port, serve `rfq_api::app()` on it in the background, and
/// return the base URL a `RfqClient` should target. The server task lives for
/// the duration of the test's runtime.
async fn spawn_api() -> Url {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, rfq_api::app()).await.unwrap();
    });
    Url::parse(&format!("http://{addr}/")).unwrap()
}

fn rgb_asset() -> AssetId {
    AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Rgb20,
        id: "rgb:eejuoPHh-agACtkj-6j2JkSs-cI4PIwm-CzKGJ7v-1s~IrX0".to_owned(),
    }
}

fn btc_asset() -> AssetId {
    AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Btc,
        id: "btc".to_owned(),
    }
}

fn rfq(side: Side) -> CreateRfqRequest {
    CreateRfqRequest {
        base_asset: rgb_asset(),
        quote_asset: btc_asset(),
        side,
        amount: 100,
    }
}

#[tokio::test]
async fn health_endpoint_round_trips() {
    let client = RfqClient::new(spawn_api().await);
    assert_eq!(client.health().await.unwrap().status, "ok");
}

#[tokio::test]
async fn buy_flow_settles_through_the_client() {
    let client = RfqClient::new(spawn_api().await);

    let quotes = client.request_quotes(rfq(Side::Buy)).await.unwrap();
    assert_eq!(quotes.len(), 1, "the mock maker returns one quote");
    let quote_id = quotes[0].quote_id.clone();

    let intent = client
        .accept_quote(AcceptQuoteRequest {
            quote_id: quote_id.clone(),
            leg: SwapLeg::Buy {
                rgb_invoice: "rgb:taker-invoice".to_owned(),
                btc_funding_addr: "bcrt1qtaker".to_owned(),
            },
        })
        .await
        .unwrap();
    assert_eq!(intent.status, SettlementStatus::AwaitingTakerSignature);
    let psbt = intent
        .transfer
        .expect("buy accept returns a partial PSBT")
        .partial_psbt;

    let settled = client
        .submit_signed_psbt(quote_id, format!("{psbt}|signed"))
        .await
        .unwrap();
    assert_eq!(settled.status, SettlementStatus::PendingBitcoinConfirm);
    assert!(settled.witness_txid.is_some());
}

#[tokio::test]
async fn sell_quote_accepts_and_rejects_a_bad_consignment() {
    let client = RfqClient::new(spawn_api().await);

    let quotes = client.request_quotes(rfq(Side::Sell)).await.unwrap();
    let quote = &quotes[0];
    assert!(
        quote.maker_rgb_invoice.is_none(),
        "provenance model: sell quotes carry no maker invoice",
    );

    let intent = client
        .accept_quote(AcceptQuoteRequest {
            quote_id: quote.quote_id.clone(),
            leg: SwapLeg::Sell {
                btc_payout_addr: "bcrt1qtaker".to_owned(),
                rgb_change_invoice: None,
            },
        })
        .await
        .unwrap();
    assert_eq!(intent.status, SettlementStatus::AwaitingConsignment);

    // Exercises `submit_consignment`: a malformed consignment is rejected.
    let result = client
        .submit_consignment(quote.quote_id.clone(), "not-a-consignment".to_owned(), vec![])
        .await;
    assert!(matches!(result, Err(RfqClientError::HttpStatus { .. })));
}

#[tokio::test]
async fn accept_unknown_quote_surfaces_an_http_error() {
    let client = RfqClient::new(spawn_api().await);

    let result = client
        .accept_quote(AcceptQuoteRequest {
            quote_id: QuoteId("no-such-quote".to_owned()),
            leg: SwapLeg::Buy {
                rgb_invoice: "rgb:x".to_owned(),
                btc_funding_addr: "bcrt1qtaker".to_owned(),
            },
        })
        .await;
    assert!(matches!(result, Err(RfqClientError::HttpStatus { .. })));
}
