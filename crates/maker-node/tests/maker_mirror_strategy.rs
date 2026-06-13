//! End-to-end test for the order FILLED counter + auto-mirror strategy.
//!
//! Seeds a mirror-enabled BUY order (maker sells RGB), drives one buy through
//! the broker so the order fills, runs the strategy loop, and asserts:
//!   - the maker recorded the fill (`filled_for(asset, Buy) == amount`), and
//!   - the strategy auto-placed the opposite SELL order at `fill_unit - spread`,
//!     sized to the filled amount, itself mirror-enabled.
//!
//! Run with the regtest stack up + tools installed (see rfq-rgb/tests/cli.rs):
//!   cargo test -p maker-node --test maker_mirror_strategy -- --ignored

use std::sync::Arc;
use std::time::Duration;

use maker_node::{
    build_runtime, maker_app, orders, spawn_chain_observer_loop, spawn_order_reload_loop,
    spawn_strategy_loop, IntervalsConfig, MakerNodeConfig, MakerNodeRuntime, MakerSection,
    RebalancePolicyConfig, RgbConfig, SignerConfig,
};
use rfq_client::{RfqClient, Url};
use rfq_rgb::test_helpers;
use rfq_router::{HttpMakerConnector, MakerConnector};
use rfq_store::OrderRecord;
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, CreateRfqRequest, MakerId, Side,
    SwapLeg,
};
use tokio::net::TcpListener;

const MAKER_NODE_ID: &str = "regtest-mirror-strategy-maker";
const BUY_AMOUNT: u64 = 100;
const UNIT_PRICE: u64 = 20; // sats per RGB unit (the seeded buy order's price)
const SPREAD_BPS: u16 = 500; // 5% — buy back cheaper

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see rfq-rgb/tests/cli.rs header"]
async fn fill_records_and_auto_mirrors_the_opposite_order() {
    let stack = test_helpers::stack().await;
    let asset = stack.asset();

    let mut config = MakerNodeConfig {
        maker: MakerSection {
            node_id: MAKER_NODE_ID.to_owned(),
            listen_addr: "127.0.0.1:0".to_owned(),
            broker_url: "http://127.0.0.1:3000".to_owned(),
        },
        intervals: IntervalsConfig {
            cleanup: Duration::from_secs(60),
            rebalance: Duration::from_secs(60),
            chain_observer: Duration::from_millis(500),
            strategy: Duration::from_millis(300),
        },
        rebalance: RebalancePolicyConfig::default(),
        rgb: Some(RgbConfig {
            network: "regtest".to_owned(),
            data_dir: stack.maker_stash_dir().to_owned(),
            wallet_name: "maker".to_owned(),
            electrum_url: stack.electrum_url().to_owned(),
            signer: SignerConfig {
                account_file: stack.maker_account_file().to_owned(),
                password: String::new(),
            },
        }),
    };

    maker_node::seed_contract_registry(&config, stack.contract_id_str())
        .await
        .expect("seed contract registry");
    // Build the runtime, seed a mirror-enabled BUY order, spawn the daemon +
    // the order-reload + strategy loops (the bits `run()` wires up).
    let runtime: MakerNodeRuntime = build_runtime(&config).await.expect("build_runtime");
    let MakerNodeRuntime {
        maker,
        chain_observer,
        order_store,
        ..
    } = runtime;
    let chain_observer = chain_observer.expect("RGB config present → chain observer must spawn");

    let buy_order = OrderRecord {
        id: "ord-mirror-buy".to_owned(),
        side: "buy".to_owned(),
        asset_id: stack.contract_id_str().to_owned(),
        price: UNIT_PRICE,
        size: 1000,
        created_at_ms: 1,
        mirror: true,
        mirror_spread_bps: SPREAD_BPS,
    };
    order_store.upsert(buy_order).await.expect("seed buy order");
    let seeded = order_store.list().await.expect("list");
    maker.reload_price_policy(orders::price_policy(&seeded));

    let app = maker_app(maker.clone());
    let listener = TcpListener::bind(&config.maker.listen_addr)
        .await
        .expect("bind maker listener");
    let bound = listener.local_addr().expect("addr");
    config.maker.listen_addr = bound.to_string();
    let maker_base_url = format!("http://{bound}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let observer =
        spawn_chain_observer_loop(maker.clone(), chain_observer, Duration::from_millis(500));
    let reload = spawn_order_reload_loop(
        maker.clone(),
        order_store.clone(),
        Duration::from_millis(300),
    );
    let strategy = spawn_strategy_loop(
        maker.clone(),
        order_store.clone(),
        config.intervals.strategy,
    );

    let broker_base_url = spawn_broker(&maker_base_url, MAKER_NODE_ID).await;
    let client = RfqClient::new(Url::parse(&broker_base_url).expect("broker url"));

    stack
        .fund_address(stack.taker_funding_addr(), "0.5")
        .expect("fund taker");
    sync_taker(stack).await;

    // Drive the buy: the maker sells BUY_AMOUNT RGB; the buy-side order fills.
    let buy_txid = drive_buy(&client, &asset, BUY_AMOUNT, stack).await;
    assert!(!buy_txid.is_empty(), "buy broadcast");
    stack.mine_block().expect("mine");

    // Give the strategy loop a few ticks to process the fill.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // ★ FILLED counter: the maker recorded BUY_AMOUNT sold on the buy side.
    let filled = maker
        .filled_for(stack.contract_id_str(), &Side::Buy, 0)
        .await;
    assert_eq!(
        filled, BUY_AMOUNT,
        "maker should record {BUY_AMOUNT} filled on the buy side (got {filled})"
    );

    // ★ Auto-mirror: a SELL order was placed at fill_unit - spread, sized to the fill.
    let mirror = order_store
        .get(stack.contract_id_str(), "sell")
        .await
        .expect("order store get")
        .expect("strategy must auto-place the opposite SELL mirror order");
    let expected_unit = UNIT_PRICE * (10_000 - SPREAD_BPS as u64) / 10_000; // 20 * 9500/10000 = 19
    assert_eq!(
        mirror.price, expected_unit,
        "mirror priced at fill_unit - spread"
    );
    assert_eq!(mirror.size, BUY_AMOUNT, "mirror sized to the filled amount");
    assert!(
        mirror.mirror,
        "mirror order is itself mirror-enabled (continues the loop)"
    );
    assert_eq!(mirror.mirror_spread_bps, SPREAD_BPS);

    observer.abort();
    reload.abort();
    strategy.abort();
    drop(maker);
}

fn btc_asset() -> AssetId {
    AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Btc,
        id: "btc".to_owned(),
    }
}

/// Slim buy driver (asserts on the maker side, so skips taker-side checks).
async fn drive_buy(
    client: &RfqClient,
    asset: &AssetId,
    amount: u64,
    stack: &test_helpers::RegtestStack,
) -> String {
    let rgb_invoice = {
        let taker = stack.taker_backend().await;
        taker.create_invoice(asset, amount).await.expect("invoice")
    };
    let quotes = client
        .request_quotes(CreateRfqRequest {
            base_asset: asset.clone(),
            quote_asset: btc_asset(),
            side: Side::Buy,
            amount,
        })
        .await
        .expect("request_quotes");
    let quote = quotes.into_iter().next().expect("maker quotes the buy");
    let accepted = client
        .accept_quote(AcceptQuoteRequest {
            quote_id: quote.quote_id.clone(),
            leg: SwapLeg::Buy {
                rgb_invoice,
                btc_funding_addr: stack.taker_funding_addr().to_owned(),
            },
        })
        .await
        .expect("accept buy");
    let transfer = accepted.transfer.expect("buy emits partial_psbt");
    let signed = {
        let taker = stack.taker_backend().await;
        taker
            .sign_and_finalize(&transfer.partial_psbt)
            .expect("sign buy")
    };
    client
        .submit_signed_psbt(quote.quote_id, signed)
        .await
        .expect("submit signed buy")
        .witness_txid
        .expect("witness txid")
}

async fn spawn_broker(maker_base_url: &str, maker_id: &str) -> String {
    let connector = HttpMakerConnector::new(
        MakerId(maker_id.to_owned()),
        Url::parse(maker_base_url).expect("maker url"),
    );
    let makers: Vec<Arc<dyn MakerConnector>> = vec![Arc::new(connector)];
    let app = rfq_api::app_with_makers(makers);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind broker");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    format!("http://{addr}")
}

async fn sync_taker(stack: &test_helpers::RegtestStack) {
    let taker = stack.taker_backend().await;
    taker.sync_wallet().await.expect("taker sync");
}
