//! Regression test for the maker's RGB-change stranding bug (the tapret
//! host-output recognition failure).
//!
//! THE BUG: the maker's RGB change/receive seal lands on the tapret
//! commitment-host output (keychain 10). That output's scriptPubkey is the
//! *tweaked* taproot key. bp-wallet's electrum scan recognizes tweaked outputs
//! only inside its gap-limit window; when `next_address` drifted the host to a
//! sparse high index, the maker's change fell past the window and was never
//! re-scanned — so after a reload + sync the maker's own RGB change was
//! invisible (dropped by `list_inventory_utxos`, which keeps only allocations
//! whose outpoint is in `wallet.utxos()`). The maker's inventory silently went
//! to zero across swaps.
//!
//! THE FIX: pin every tapret host to a fixed LOW index (`pinned_tapret_host_addr`
//! in rfq-rgb/src/swap.rs), so it's always inside the scan window and always
//! re-recognized.
//!
//! THE TEST: drive one buy (the maker SELLS RGB; its change lands on the pinned
//! host), stop the maker daemon, then re-open the maker wallet fresh, sync it
//! (the incremental `update()` path where stranding happened), and assert the
//! change is recognized:
//!   - `list_inventory_utxos` contains an outpoint on the swap tx, AND
//!   - the maker's recognized RGB total dropped by EXACTLY the amount sold —
//!     not by the whole consumed input (which is what a stranded change looks
//!     like: the consumed UTXO gone, its change invisible).
//!
//! Pre-fix this fails (post total collapses to ~0, no change outpoint); with the
//! fix it passes.
//!
//! Run with the regtest stack up + tools installed (see rfq-rgb/tests/cli.rs):
//!   cargo test -p maker-node --test maker_tapret_change_recognized -- --ignored

use std::sync::Arc;
use std::time::Duration;

use maker_node::{
    build_runtime, maker_app, spawn_chain_observer_loop, IntervalsConfig, MakerNodeConfig,
    MakerNodeRuntime, MakerSection, RebalancePolicyConfig, RgbConfig, SignerConfig,
};
use rfq_client::{RfqClient, Url};
use rfq_rgb::test_helpers;
use rfq_rgb::RgbBackend;
use rfq_router::{HttpMakerConnector, MakerConnector};
use rfq_types::{
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, CreateRfqRequest, MakerId, Side,
    SwapLeg,
};
use tokio::net::TcpListener;

const MAKER_NODE_ID: &str = "regtest-tapret-change-maker";

/// How much RGB the taker buys (i.e. the maker sells) in the leg under test.
const BUY_AMOUNT: u64 = 100;

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see rfq-rgb/tests/cli.rs header"]
async fn maker_recognizes_pinned_tapret_change_after_swap() {
    let stack = test_helpers::stack().await;
    let asset = stack.asset();

    // --- Maker RGB inventory BEFORE any swap (daemon not yet running, so the
    //     harness backend has exclusive access to the wallet files) ---
    let pre_total = maker_rgb_total(stack, &asset).await;
    assert!(
        pre_total >= BUY_AMOUNT,
        "maker must hold at least {BUY_AMOUNT} RGB to sell (has {pre_total})"
    );

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
    let (maker, observer_handle, maker_base_url) = spawn_maker_node(&mut config).await;
    // No standing order ⇒ the maker declines; seed a flat both-sides policy.
    maker.reload_price_policy(maker_node::orders::flat_policy(
        stack.contract_id_str(),
        101,
        1_000_000_000,
    ));
    let broker_base_url = spawn_broker(&maker_base_url, MAKER_NODE_ID).await;
    let client = RfqClient::new(Url::parse(&broker_base_url).expect("broker url parses"));

    // The taker pays BTC for the buy; fund its keychain-0 funding address.
    stack
        .fund_address(stack.taker_funding_addr(), "0.5")
        .expect("pre-fund taker funding addr");
    sync_taker(stack).await;

    // --- The leg under test: taker buys `BUY_AMOUNT`, maker sells; the maker's
    //     RGB change lands on the pinned tapret host output ---
    let buy_txid = drive_buy(&client, &asset, BUY_AMOUNT, stack).await;
    assert!(!buy_txid.is_empty(), "buy broadcast a witness tx");
    stack.mine_block().expect("mine the swap tx");

    // Stop the daemon (and its background observer) so it isn't touching the
    // wallet files while we re-open them: the stock lock is per-instance, so the
    // daemon's backend and the harness backend don't share it.
    observer_handle.abort();
    drop(maker);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // --- Re-open the maker wallet fresh, run the incremental sync (the exact
    //     path where the change used to strand), and assert recognition ---
    let (post_total, change_outpoint, change_terminal) = {
        let maker = stack.maker_backend().await;
        maker
            .sync_wallet()
            .await
            .expect("maker sync after the swap");
        let inv = maker
            .list_inventory_utxos(&asset)
            .await
            .expect("maker inventory after the swap");
        let change = inv.iter().find(|u| u.outpoint.txid == buy_txid).cloned();
        let total: u64 = inv.iter().map(|u| u.amount).sum();
        let terminal = match &change {
            Some(u) => maker
                .debug_outpoint_terminal(&u.outpoint)
                .await
                .expect("terminal lookup"),
            None => None,
        };
        (total, change, terminal)
    };

    // ★ Recognition: the change on the pinned host is a tracked inventory UTXO.
    assert!(
        change_outpoint.is_some(),
        "maker must recognize its RGB change on the pinned tapret host (an inventory \
         outpoint on the swap tx {buy_txid}); a missing one means the change stranded"
    );
    // ★ Exact balance: total drops by ONLY the amount sold — not the whole consumed
    //   input (which is what a stranded change looks like).
    assert_eq!(
        post_total,
        pre_total - BUY_AMOUNT,
        "maker's recognized RGB should drop by EXACTLY the {BUY_AMOUNT} sold \
         (pre {pre_total}); a larger drop means the change output stranded and only \
         the consumed input's disappearance is visible"
    );
    // ★ Pinning mechanism: the change lands on the FIXED low host terminal
    //   (keychain 10, index 0), not a `next_address`-advanced one. This is the
    //   deterministic distinguisher from the pre-fix behavior, which advanced the
    //   index per swap and eventually drifted the host past the scan's gap limit.
    assert_eq!(
        change_terminal,
        Some((10, 0)),
        "the maker's tapret change must land on the pinned host terminal &10/0 \
         (got {change_terminal:?}); a different index means the host is still being \
         advanced by next_address rather than pinned"
    );
}

/// The maker's currently-recognized RGB total (sum over `list_inventory_utxos`,
/// i.e. stock allocations whose outpoint bp-wallet tracks as a live UTXO).
/// Syncs first so a freshly-bootstrapped wallet's coins are in the cache.
async fn maker_rgb_total(stack: &test_helpers::RegtestStack, asset: &AssetId) -> u64 {
    let maker = stack.maker_backend().await;
    maker.sync_wallet().await.expect("maker sync (pre-swap)");
    maker
        .list_inventory_utxos(asset)
        .await
        .expect("maker inventory (pre-swap)")
        .iter()
        .map(|u| u.amount)
        .sum()
}

fn btc_asset() -> AssetId {
    AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Btc,
        id: "btc".to_owned(),
    }
}

/// Drive a buy through the broker: taker mints the receiving invoice, declares
/// its BTC funding address, signs the maker-built PSBT, maker broadcasts at
/// `/sign`. Returns the witness txid. (Slimmer than `broker_round_trip`'s
/// driver — this test asserts on the MAKER side, so it skips the taker-side
/// landing checks.)
async fn drive_buy(
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

    client
        .submit_signed_psbt(quote.quote_id, signed)
        .await
        .expect("submit signed buy")
        .witness_txid
        .expect("buy settled intent carries the broadcast witness txid")
}

/// Bind the maker HTTP server to a random port, spawn `axum::serve` + the chain
/// observer loop. Mirrors `broker_round_trip.rs`.
async fn spawn_maker_node(
    config: &mut MakerNodeConfig,
) -> (rfq_maker::Maker, tokio::task::JoinHandle<()>, String) {
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
        .expect("bind maker listener");
    let bound_addr = listener.local_addr().expect("maker local_addr");
    config.maker.listen_addr = bound_addr.to_string();
    let base_url = format!("http://{bound_addr}");

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let observer_handle = spawn_chain_observer_loop(
        maker.clone(),
        chain_observer,
        config.intervals.chain_observer,
    );

    (maker, observer_handle, base_url)
}

/// Spin up the broker in-process with a single `HttpMakerConnector` pointed at
/// the maker daemon. Returns the broker base URL. Mirrors `broker_round_trip.rs`.
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

/// Refresh the taker's bp-wallet cache so newly-confirmed UTXOs become visible.
async fn sync_taker(stack: &test_helpers::RegtestStack) {
    let taker = stack.taker_backend().await;
    taker.sync_wallet().await.expect("taker sync_wallet");
}
