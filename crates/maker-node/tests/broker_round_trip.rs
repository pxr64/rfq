//! End-to-end integration test routing a full BUY and a full SELL
//! BTC↔RGB swap through the broker (`rfq-api`) to a remote maker daemon
//! (`maker_app`), against the live regtest stack. This is the first test
//! that exercises the complete `taker → broker → remote-maker` path with
//! the real `LibRgbBackend` on both maker and taker sides.
//!
//! The taker side is the production `rfq_rgb::Taker` (via the harness's
//! `TakerGuard`, which delegates to it). The broker is spun up in-process
//! with a single `HttpMakerConnector` pointed at the maker daemon — the
//! test analog of `colorex broker up`.
//!
//! The SELL leg additionally gates the in-process consignment builder
//! (`Taker::create_transfer_to_invoice`, i.e. `wallet.pay`): if it passes,
//! the maker accepts a consignment built without the `rgb` CLI shell-out.
//!
//! Run with the regtest stack up + tools installed (see
//! `rfq-rgb/tests/cli.rs` header):
//!   cargo test -p maker-node --test broker_round_trip -- --ignored

use std::sync::Arc;
use std::time::Duration;

use maker_node::{
    build_runtime, maker_app, spawn_chain_observer_loop, IntervalsConfig, MakerNodeConfig,
    MakerNodeRuntime, MakerSection, RebalancePolicyConfig, RgbConfig, SignerConfig,
};
use rfq_client::{RfqClient, Url};
use rfq_router::{HttpMakerConnector, MakerConnector};
use rfq_rgb::test_helpers;
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, CreateRfqRequest, MakerId, Side,
    SwapLeg,
};
use tokio::net::TcpListener;

/// Must match the `HttpMakerConnector`'s `MakerId` — the broker routes
/// accept/consignment/sign by matching the stored `quote.maker_id`.
const MAKER_NODE_ID: &str = "regtest-broker-test-maker";

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see rfq-rgb/tests/cli.rs header"]
async fn broker_routes_buy_and_sell_to_remote_maker() {
    let stack = test_helpers::stack().await;
    let asset = stack.asset();

    let mut config = MakerNodeConfig {
        maker: MakerSection {
            node_id: MAKER_NODE_ID.to_owned(),
            listen_addr: "127.0.0.1:0".to_owned(), // resolved post-bind
            broker_url: "http://127.0.0.1:3000".to_owned(),
        },
        intervals: IntervalsConfig {
            cleanup: Duration::from_secs(60),
            rebalance: Duration::from_secs(60),
            chain_observer: Duration::from_millis(500),
        },
        rebalance: RebalancePolicyConfig::default(),
        rgb: Some(RgbConfig {
            network: "regtest".to_owned(),
            data_dir: stack.maker_stash_dir().to_owned(),
            wallet_name: "maker".to_owned(),
            electrum_url: stack.electrum_url().to_owned(),
            contract_id: stack.contract_id_str().to_owned(),
            signer: SignerConfig {
                account_file: stack.maker_account_file().to_owned(),
                password: String::new(),
            },
        }),
    };

    // Maker daemon (HTTP + chain observer) on a random port.
    let (maker, observer_handle, maker_base_url) = spawn_maker_node(&mut config).await;

    // Broker in-process, routing to the maker over HTTP.
    let broker_base_url = spawn_broker(&maker_base_url, MAKER_NODE_ID).await;
    let client = RfqClient::new(Url::parse(&broker_base_url).expect("broker url parses"));

    // Pre-fund the taker's keychain-0 funding address for the buy leg (the
    // bootstrap only funds keychain-10). The maker's buy path scans this
    // address via `list_unspent`.
    stack
        .fund_address(stack.taker_funding_addr(), "0.5")
        .expect("pre-fund taker funding addr");
    sync_taker(stack).await;

    // --- BUY: taker buys 100 RGB, paying BTC, routed through the broker ---
    let buy_txid = drive_buy_via_broker(&client, &asset, 100, stack).await;
    assert!(!buy_txid.is_empty(), "buy broadcast a witness tx");

    // Settle the buy so balances confirm before the sell.
    stack.mine_block().expect("mine after buy");
    wait_for_observer(config.intervals.chain_observer).await;
    sync_taker(stack).await;

    // --- SELL: taker sells 50 RGB, receiving BTC, routed through the broker
    let sell_txid = drive_sell_via_broker(&client, &asset, 50, stack).await;
    assert!(!sell_txid.is_empty(), "sell broadcast a witness tx");
    assert_ne!(buy_txid, sell_txid, "buy and sell are distinct broadcasts");

    observer_handle.abort();
    drop(maker);
}

fn btc_asset() -> AssetId {
    AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Btc,
        id: "btc".to_owned(),
    }
}

/// Buy `amount` RGB through the broker. The taker mints the receiving RGB
/// invoice, declares its BTC funding address, signs the maker-built PSBT,
/// and the maker broadcasts at `/sign`. Returns the witness txid.
async fn drive_buy_via_broker(
    client: &RfqClient,
    asset: &AssetId,
    amount: u64,
    stack: &test_helpers::RegtestStack,
) -> String {
    let rgb_invoice = {
        let taker = stack.taker_backend().await;
        taker
            .create_invoice(asset, amount)
            .await
            .expect("taker create_invoice for buy")
    };

    let quotes = client
        .request_quotes(CreateRfqRequest {
            base_asset: asset.clone(),
            quote_asset: btc_asset(),
            side: Side::Buy,
            amount,
        })
        .await
        .expect("request_quotes buy");
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
    let transfer = accepted
        .transfer
        .expect("buy accept emits SwapTransfer with partial_psbt");

    let signed = {
        let taker = stack.taker_backend().await;
        taker
            .sign_and_finalize(&transfer.partial_psbt)
            .expect("taker sign+finalize buy")
    };

    let settled = client
        .submit_signed_psbt(quote.quote_id, signed)
        .await
        .expect("submit signed buy");
    settled
        .witness_txid
        .expect("buy settled intent carries the broadcast witness txid")
}

/// Sell `amount` RGB through the broker. The maker publishes its RGB invoice
/// on the quote; the taker builds an in-process consignment to it
/// (`Taker::create_transfer_to_invoice`), delivers it at `/consignment`, then
/// signs the maker-built swap PSBT. Returns the witness txid.
async fn drive_sell_via_broker(
    client: &RfqClient,
    asset: &AssetId,
    amount: u64,
    stack: &test_helpers::RegtestStack,
) -> String {
    let quotes = client
        .request_quotes(CreateRfqRequest {
            base_asset: asset.clone(),
            quote_asset: btc_asset(),
            side: Side::Sell,
            amount,
        })
        .await
        .expect("request_quotes sell");
    let quote = quotes.into_iter().next().expect("maker quotes the sell");
    // TODO(provenance): this e2e still drives the old pay-to-invoice flow. Under the
    // provenance model (docs/provenance-consignment-proposal.md) the sell quote
    // carries NO maker invoice; convert this to `export_provenance` + named outpoints
    // when running Task 4 on regtest. Compiles today; would need conversion to pass.
    let maker_rgb_invoice = quote.maker_rgb_invoice.clone().unwrap_or_default();

    // The taker's RGB input likely exceeds `amount`; supply a change invoice
    // so the maker routes the surplus back. Its amount field is ignored — the
    // maker only reads the beneficiary seal off it (see cli.rs sell test).
    let rgb_change_invoice = {
        let taker = stack.taker_backend().await;
        taker
            .create_invoice(asset, amount)
            .await
            .expect("taker create_invoice (change)")
    };

    let accepted = client
        .accept_quote(AcceptQuoteRequest {
            quote_id: quote.quote_id.clone(),
            leg: SwapLeg::Sell {
                btc_payout_addr: stack.taker_payout_addr().to_owned(),
                rgb_change_invoice: Some(rgb_change_invoice),
            },
        })
        .await
        .expect("accept sell");
    assert!(
        accepted.transfer.is_none(),
        "sell accept awaits the consignment before emitting a PSBT"
    );

    // Build the consignment in-process (the gate on wallet.pay).
    let consignment = {
        let taker = stack.taker_backend().await;
        taker
            .create_transfer_to_invoice(&maker_rgb_invoice, 1_000)
            .await
            .expect("taker create_transfer_to_invoice")
    };

    let delivered = client
        .submit_consignment(quote.quote_id.clone(), consignment, vec![])
        .await
        .expect("submit consignment");
    let transfer = delivered
        .transfer
        .expect("consignment delivery emits the maker-built swap PSBT");

    let signed = {
        let taker = stack.taker_backend().await;
        taker
            .sign_and_finalize(&transfer.partial_psbt)
            .expect("taker sign+finalize sell")
    };

    let settled = client
        .submit_signed_psbt(quote.quote_id, signed)
        .await
        .expect("submit signed sell");
    settled
        .witness_txid
        .expect("sell settled intent carries the broadcast witness txid")
}

/// Bind the maker HTTP server to a random port, spawn `axum::serve` + the
/// chain observer loop. Mirrors the helper in `regtest_http_round_trip.rs`.
async fn spawn_maker_node(
    config: &mut MakerNodeConfig,
) -> (rfq_maker::Maker, tokio::task::JoinHandle<()>, String) {
    let runtime: MakerNodeRuntime = build_runtime(config).await.expect("build_runtime");
    let MakerNodeRuntime {
        maker,
        chain_observer,
    } = runtime;
    let chain_observer = chain_observer.expect("RGB config present → chain observer must spawn");

    let app = maker_app(maker.clone());
    let listener = TcpListener::bind(&config.maker.listen_addr)
        .await
        .expect("bind maker listener");
    let bound_addr = listener.local_addr().expect("maker local_addr");
    config.maker.listen_addr = bound_addr.to_string();
    let base_url = format!("http://{bound_addr}");

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let observer_handle =
        spawn_chain_observer_loop(maker.clone(), chain_observer, config.intervals.chain_observer);

    (maker, observer_handle, base_url)
}

/// Spin up the broker in-process with a single `HttpMakerConnector` pointed at
/// the maker daemon. Returns the broker base URL.
async fn spawn_broker(maker_base_url: &str, maker_id: &str) -> String {
    let connector = HttpMakerConnector::new(
        MakerId(maker_id.to_owned()),
        Url::parse(maker_base_url).expect("maker url parses"),
    );
    let makers: Vec<Arc<dyn MakerConnector>> = vec![Arc::new(connector)];
    let app = rfq_api::app_with_makers(makers);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind broker listener");
    let addr = listener.local_addr().expect("broker local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    format!("http://{addr}")
}

/// Sleep long enough for the chain observer to tick at least twice.
async fn wait_for_observer(interval: Duration) {
    tokio::time::sleep(interval * 3).await;
}

/// Refresh the taker's bp-wallet cache so newly-confirmed UTXOs become visible.
async fn sync_taker(stack: &test_helpers::RegtestStack) {
    let taker = stack.taker_backend().await;
    taker.sync_wallet().await.expect("taker sync_wallet");
}
