//! Auto-discovery round trip over the WebSocket, fully mock-backed (no regtest,
//! not `#[ignore]`): a maker dials the broker and registers, then a taker drives
//! a buy through the broker — every maker call (`request_quote`, `accept_quote`)
//! traverses the WebSocket via `WsMakerConnector`. Proves the registry +
//! register-on-connect + request/response correlation end to end.

use std::time::Duration;

use maker_node::broker_client::{broker_ws_url, run_broker_stream};
use maker_node::{build_runtime, IntervalsConfig, MakerNodeConfig, MakerSection, RebalancePolicyConfig};
use rfq_api::registry::MakerRegistry;
use rfq_client::{RfqClient, Url};
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, CreateRfqRequest, MakerId, Side, SwapLeg,
};
use tokio::net::TcpListener;

const NODE_ID: &str = "ws-test-maker";

fn rgb_asset() -> AssetId {
    AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Rgb20,
        id: "rgb-test-asset".to_owned(), // matches build_runtime's mock asset id
    }
}

fn btc_asset() -> AssetId {
    AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Btc,
        id: "btc".to_owned(),
    }
}

#[tokio::test]
async fn maker_autoregisters_then_quotes_and_accepts_over_ws() {
    // Mock maker (rgb: None → MockRgbBackend + MockBitcoinClient seeded for the
    // buy side: RGB inventory to sell + the taker's funding address).
    let config = MakerNodeConfig {
        maker: MakerSection {
            node_id: NODE_ID.to_owned(),
            listen_addr: "127.0.0.1:0".to_owned(),
            broker_url: "http://127.0.0.1:3000".to_owned(),
        },
        intervals: IntervalsConfig {
            cleanup: Duration::from_secs(60),
            rebalance: Duration::from_secs(60),
            chain_observer: Duration::from_secs(60),
            strategy: Duration::from_millis(500),
        },
        rebalance: RebalancePolicyConfig::default(),
        rgb: None,
    };
    let maker = build_runtime(&config).await.expect("build_runtime").maker;
    // No standing order ⇒ the maker declines; seed a flat policy for the mock asset.
    maker.reload_price_policy(maker_node::orders::flat_policy(
        "rgb-test-asset",
        101,
        1_000_000_000,
    ));

    // Broker on a random port with an empty registry — the maker dials in.
    let registry = MakerRegistry::new();
    let app = rfq_api::app_with_registry(registry.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let broker_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Maker WS client dials the broker and registers.
    let ws_url = broker_ws_url(&format!("http://{broker_addr}"));
    tokio::spawn(run_broker_stream(
        ws_url,
        MakerId(NODE_ID.to_owned()),
        maker,
    ));

    // Wait for the registration to land in the registry.
    let mut registered = false;
    for _ in 0..100 {
        if registry.get(&MakerId(NODE_ID.to_owned())).await.is_some() {
            registered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(registered, "maker should auto-register over the websocket");

    let client = RfqClient::new(Url::parse(&format!("http://{broker_addr}")).unwrap());
    let asset = rgb_asset();

    // request_quote over WS.
    let quotes = client
        .request_quotes(CreateRfqRequest {
            base_asset: asset.clone(),
            quote_asset: btc_asset(),
            side: Side::Buy,
            amount: 100,
        })
        .await
        .expect("request_quotes");
    let quote = quotes
        .into_iter()
        .next()
        .expect("maker quoted the buy over the websocket");
    assert_eq!(quote.maker_id, MakerId(NODE_ID.to_owned()));

    // accept_quote over WS — proves the settlement-call correlation path too.
    let accepted = client
        .accept_quote(AcceptQuoteRequest {
            quote_id: quote.quote_id.clone(),
            leg: SwapLeg::Buy {
                rgb_invoice: "rgb:mock-invoice:rgb-test-asset:100:0".to_owned(),
                btc_funding_addr: "bcrt1qtaker".to_owned(),
            },
        })
        .await
        .expect("accept over the websocket");
    assert_eq!(accepted.quote_id, quote.quote_id);
}
