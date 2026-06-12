//! HTTP-layer integration test for #28: drives two consecutive buy
//! round-trips through `maker_node::maker_app` against the live regtest
//! stack. Covers the wiring the rfq-rgb-layer
//! `chain_observer_enables_consecutive_buys` test (#93750b4) skips:
//! axum routing, JSON request/response shapes, the actual
//! `spawn_chain_observer_loop` task running in the background.

use std::time::Duration;

use maker_node::{
    build_runtime, maker_app, spawn_chain_observer_loop, IntervalsConfig, MakerNodeConfig,
    MakerNodeRuntime, MakerSection, RebalancePolicyConfig, RgbConfig, SignerConfig,
};
use rfq_rgb::test_helpers;
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, Quote, QuoteRequest, RfqId,
    SettlementIntent, Side, SwapLeg,
};
use tokio::net::TcpListener;

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see rfq-rgb/tests/cli.rs header"]
async fn http_two_consecutive_buys_via_chain_observer() {
    let stack = test_helpers::stack().await;
    let asset = stack.asset();

    // --- Build a real-RGB-backed maker config pointed at the bootstrapped
    //     maker stash -------------------------------------------------------
    let mut config = MakerNodeConfig {
        maker: MakerSection {
            node_id: "regtest-http-test".to_owned(),
            listen_addr: "127.0.0.1:0".to_owned(), // resolved post-bind
            broker_url: "http://127.0.0.1:3000".to_owned(),
        },
        intervals: IntervalsConfig {
            cleanup: Duration::from_secs(60),
            rebalance: Duration::from_secs(60),
            // Aggressive observer cadence so the test doesn't have to wait 5s
            // between swaps. 500ms is fast enough that the wait_for_observer
            // sleep below catches the post-broadcast tick.
            chain_observer: Duration::from_millis(500),
            strategy: Duration::from_millis(500),
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
    // --- Spin up the maker_app on a random port + spawn the chain observer
    let (maker, observer_handle, base_url) = spawn_maker_node(&mut config).await;

    // --- Pre-fund the taker's btc_funding_addr (keychain-0). The bootstrap
    //     only funds keychain-10 via `fund_role`; the maker's HTTP path
    //     calls `list_unspent(funding_addr)` and would otherwise see an
    //     empty UTXO set + reject the accept with "insufficient BTC".
    stack
        .fund_address(stack.taker_funding_addr(), "0.5")
        .expect("pre-fund taker funding addr");
    sync_taker(stack).await; // bp-wallet cache must see the new UTXO too

    // --- Drive buy #1 through the HTTP surface ----------------------------
    let client = reqwest::Client::new();
    let txid1 = drive_buy(&client, &base_url, &asset, "rfq-1", 100, stack).await;

    // Mine + wait for chain observer to tick. Maker's wallet refreshes
    // via the observer loop; the taker has no such loop here, so we
    // sync it manually before driving buy #2 (it needs to see its BTC
    // change UTXO from buy #1).
    stack.mine_block().expect("mine block #1");
    wait_for_observer(config.intervals.chain_observer).await;
    sync_taker(stack).await;

    // --- Drive buy #2 — should reuse the RGB change UTXO from buy #1 -----
    let txid2 = drive_buy(&client, &base_url, &asset, "rfq-2", 50, stack).await;
    assert_ne!(txid1, txid2, "two distinct broadcasts");

    // Mine + wait so the chain observer ingests buy #2's RGB change UTXO as
    // Available — without this, the inventory snapshot below sees the
    // pre-buy-#2 change in PendingBitcoinConfirm and the post-buy-#2 change
    // not yet in the store at all.
    stack.mine_block().expect("mine block #2");
    wait_for_observer(config.intervals.chain_observer).await;

    // Confirm via inventory: maker should still have RGB allocation after
    // two consecutive swaps (the change from buy #2).
    let snap: serde_json::Value = client
        .get(format!("{base_url}/inventory"))
        .send()
        .await
        .expect("GET /inventory")
        .json()
        .await
        .expect("inventory json");
    let available = snap["available_amount"]
        .as_u64()
        .expect("available_amount field");
    assert!(
        available > 0,
        "post-two-buys maker should have remaining RGB; snapshot: {snap}"
    );

    // Clean up background tasks.
    observer_handle.abort();
    drop(maker);
}

/// Bind the maker HTTP server to a random port, spawn `axum::serve` +
/// the chain observer loop. Returns the (maker, observer JoinHandle,
/// base URL). The caller aborts the observer when the test ends.
async fn spawn_maker_node(
    config: &mut MakerNodeConfig,
) -> (
    rfq_maker::Maker,
    tokio::task::JoinHandle<()>,
    String,
) {
    let runtime: MakerNodeRuntime = build_runtime(config).await.expect("build_runtime");
    let MakerNodeRuntime {
        maker,
        chain_observer,
        ..
    } = runtime;
    let chain_observer = chain_observer.expect("RGB config present → chain observer must spawn");

    let app = maker_app(maker.clone());
    let listener = TcpListener::bind(&config.maker.listen_addr)
        .await
        .expect("bind listener");
    let bound_addr = listener.local_addr().expect("local_addr");
    config.maker.listen_addr = bound_addr.to_string();
    let base_url = format!("http://{bound_addr}");

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // Briefly yield so the server's accept loop is ready before the test
    // starts hammering it.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let observer_handle = spawn_chain_observer_loop(
        maker.clone(),
        chain_observer,
        config.intervals.chain_observer,
    );

    (maker, observer_handle, base_url)
}

/// Drive a single buy round-trip through the maker's HTTP surface +
/// the taker harness. Returns the broadcast witness txid.
async fn drive_buy(
    client: &reqwest::Client,
    base_url: &str,
    asset: &AssetId,
    rfq_id: &str,
    amount: u64,
    stack: &test_helpers::RegtestStack,
) -> String {
    // 1. Taker mints an RGB invoice to receive the bought tokens.
    let rgb_invoice = {
        let taker = stack.taker_backend().await;
        taker
            .create_invoice(asset, amount)
            .await
            .expect("taker create_invoice for buy")
    };

    // 2. POST /quotes → maker quotes the buy.
    let req = QuoteRequest {
        rfq_id: RfqId(rfq_id.to_owned()),
        base_asset: asset.clone(),
        quote_asset: AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Btc,
            id: "btc".to_owned(),
        },
        side: Side::Buy,
        amount,
        created_at_ms: 1,
    };
    let quote: Quote = client
        .post(format!("{base_url}/quotes"))
        .json(&req)
        .send()
        .await
        .expect("POST /quotes")
        .json::<Option<Quote>>()
        .await
        .expect("quote json")
        .expect("maker quotes the buy");

    // 3. POST /quotes/{id}/accept with taker's invoice + funding addr.
    // The maker's own pricing logic populated quote.price + fees; we just
    // pass that through.
    let accept_req = AcceptQuoteRequest {
        quote_id: quote.quote_id.clone(),
        leg: SwapLeg::Buy {
            rgb_invoice,
            btc_funding_addr: stack.taker_funding_addr().to_owned(),
        },
    };
    let accepted: SettlementIntent = decode_or_panic(
        client
            .post(format!("{base_url}/quotes/{}/accept", quote.quote_id.0))
            .json(&accept_req)
            .send()
            .await
            .expect("POST /accept"),
        "accept",
    )
    .await;
    let transfer = accepted
        .transfer
        .expect("accept emits SwapTransfer with partial_psbt");

    // 4. Taker signs+finalizes its inputs in the partial PSBT.
    let signed_psbt = {
        let taker = stack.taker_backend().await;
        taker
            .sign_and_finalize(&transfer.partial_psbt)
            .expect("taker sign+finalize")
    };

    // 5. POST /quotes/{id}/sign → maker finalizes its inputs + broadcasts.
    #[derive(serde::Serialize)]
    struct SignBody {
        signed_psbt: String,
    }
    let settled: SettlementIntent = decode_or_panic(
        client
            .post(format!("{base_url}/quotes/{}/sign", quote.quote_id.0))
            .json(&SignBody { signed_psbt })
            .send()
            .await
            .expect("POST /sign"),
        "sign",
    )
    .await;

    settled
        .witness_txid
        .expect("settled intent carries the broadcast witness txid")
}

/// Sleep long enough for the chain observer to tick at least twice
/// (covers the initial "skip the immediate tick" + one real tick).
async fn wait_for_observer(interval: Duration) {
    tokio::time::sleep(interval * 3).await;
}

/// Refresh the taker's bp-wallet cache so newly-confirmed UTXOs (just-funded
/// addresses, post-broadcast change) become visible to `sign_and_finalize`.
/// The maker has the chain-observer loop for this; the taker doesn't.
async fn sync_taker(stack: &test_helpers::RegtestStack) {
    let taker = stack.taker_backend().await;
    taker.sync_wallet().await.expect("taker sync_wallet");
}

/// Decode a JSON response, panicking with the raw body if the status
/// isn't 2xx or the body fails to parse as `T`. Without this, reqwest's
/// `.json::<T>().await` swallows the body and reports a generic
/// "expected value" decode error.
async fn decode_or_panic<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    label: &str,
) -> T {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        panic!("{label}: HTTP {status}: {body}");
    }
    serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("{label}: parse JSON failed ({e}); body was: {body}"))
}

