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
    AcceptQuoteRequest, AssetId, AssetKind, BitcoinNetwork, CreateRfqRequest, MakerId, Outpoint,
    Side, SwapLeg,
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

    // Register the contract so build_runtime seeds its inventory (the registry
    // replaced the old [rgb] contract_id config field).
    maker_node::seed_contract_registry(&config, stack.contract_id_str())
        .await
        .expect("seed contract registry");
    // Maker daemon (HTTP + chain observer) on a random port.
    let (maker, observer_handle, maker_base_url) = spawn_maker_node(&mut config).await;
    // The maker declines any (asset, side) with no standing order — seed a flat
    // both-sides policy so the buy + sell legs quote.
    maker.reload_price_policy(maker_node::orders::flat_policy(
        stack.contract_id_str(),
        101,
        1_000_000_000,
    ));

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
    // Snapshot the taker's RGB before the buy, to assert the bought amount lands.
    let pre_total: u64 = {
        let taker = stack.taker_backend().await;
        taker
            .inventory(asset)
            .await
            .expect("taker inventory (pre-buy)")
            .iter()
            .map(|u| u.amount)
            .sum()
    };
    // Snapshot BTC at the taker's funding/change address — the buy returns the
    // taker's BTC change here (keychain-0), funded from keychain-10 inputs.
    let funding_addr = stack.taker_funding_addr().to_owned();
    let btc_funding_before = stack.address_btc_sats(&funding_addr);

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
    let txid = settled
        .witness_txid
        .clone()
        .expect("buy settled intent carries the broadcast witness txid");

    // ★ Delivery-landing assertion: the maker's `final_consignment` delivers the
    // bought RGB to the taker's invoice seal. Mine the swap tx, import it, re-sync,
    // and confirm the taker now holds pre_total + amount. The buy mirror of the
    // sell's change-landing check — the RGB lands where it's supposed to.
    let delivery = settled
        .final_consignment
        .clone()
        .expect("a buy must return the maker-emitted RGB delivery consignment");
    stack.mine_block().expect("mine the swap tx");

    // ★ BTC payment + change assertion: the taker funds the buy from its funding
    // address and the surplus returns there as change, so the NET cost is just
    // price + fee — not a whole consumed UTXO. A lost/misrouted change output would
    // show up as a far larger drop.
    let btc_funding_after = stack.address_btc_sats(&funding_addr);
    let spent = btc_funding_before.saturating_sub(btc_funding_after);
    assert!(
        btc_funding_after < btc_funding_before,
        "taker should pay BTC for the buy ({btc_funding_before} -> {btc_funding_after})"
    );
    assert!(
        spent >= quote.price && spent <= quote.price + 100_000,
        "buy BTC cost {spent} should be ~price {} + fee (change must return to the \
         funding address — a lost change output would consume the whole input)",
        quote.price
    );
    {
        let taker = stack.taker_backend().await;
        taker
            .accept_consignment(asset, &delivery)
            .await
            .expect("taker imports the bought RGB");
        taker.sync_wallet().await.expect("taker re-sync after the buy");
        let post_total: u64 = taker
            .inventory(asset)
            .await
            .expect("taker inventory (post-buy)")
            .iter()
            .map(|u| u.amount)
            .sum();
        assert_eq!(
            post_total,
            pre_total + amount,
            "taker's RGB after a buy should be pre_total + amount bought"
        );
    }
    txid
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
    // Provenance model: the sell quote carries no maker invoice.
    assert!(quote.maker_rgb_invoice.is_none());

    // Snapshot the taker's RGB before the sell, and pick the outpoints to sell
    // (largest-first, cover `amount`). The maker spends these into the swap tx — it
    // learns them from us, named on the wire; the consignment proves their provenance.
    let (pre_total, consignment, outpoints) = {
        let taker = stack.taker_backend().await;
        let inv = taker.inventory(asset).await.expect("taker inventory");
        let pre_total: u64 = inv.iter().map(|u| u.amount).sum();
        let mut chosen: Vec<Outpoint> = Vec::new();
        let mut have = 0u64;
        for u in &inv {
            if have >= amount {
                break;
            }
            have += u.amount;
            chosen.push(u.outpoint.clone());
        }
        assert!(have >= amount, "taker holds {have} RGB, needs {amount} to sell");
        let strs: Vec<String> = chosen.iter().map(|o| format!("{}:{}", o.txid, o.vout)).collect();
        let c = taker
            .export_provenance(stack.contract_id_str(), &strs)
            .expect("export_provenance");
        (pre_total, c, chosen)
    };

    // Change invoice: the maker routes the surplus (have − amount) back to this seal.
    let rgb_change_invoice = {
        let taker = stack.taker_backend().await;
        taker
            .create_invoice(asset, amount)
            .await
            .expect("taker create_invoice (change)")
    };

    // Snapshot BTC at the taker's payout address — the sell pays the BTC proceeds here.
    let payout_addr = stack.taker_payout_addr().to_owned();
    let btc_payout_before = stack.address_btc_sats(&payout_addr);

    let accepted = client
        .accept_quote(AcceptQuoteRequest {
            quote_id: quote.quote_id.clone(),
            leg: SwapLeg::Sell {
                btc_payout_addr: payout_addr.clone(),
                rgb_change_invoice: Some(rgb_change_invoice),
            },
        })
        .await
        .expect("accept sell");
    assert!(
        accepted.transfer.is_none(),
        "sell accept awaits the consignment before emitting a PSBT"
    );

    let delivered = client
        .submit_consignment(quote.quote_id.clone(), consignment, outpoints)
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
    let txid = settled
        .witness_txid
        .clone()
        .expect("sell settled intent carries the broadcast witness txid");

    stack.mine_block().expect("mine the swap tx");

    // ★ BTC-payout assertion: the taker's BTC sale proceeds land at its payout address.
    let btc_payout_after = stack.address_btc_sats(&payout_addr);
    assert!(
        btc_payout_after > btc_payout_before,
        "sell should pay the taker's BTC proceeds to its payout address \
         ({btc_payout_before} -> {btc_payout_after})"
    );

    // ★ RGB change-landing assertion: the maker emits a `final_consignment` for the
    // surplus RGB (pre_total − amount) addressed to the taker's change seal. Import it,
    // re-sync, and confirm the taker now holds exactly the change. This is "the change
    // lands where it's supposed to," proven end-to-end.
    if pre_total > amount {
        let change = settled
            .final_consignment
            .clone()
            .expect("a surplus sell must return the maker-emitted change consignment");
        let taker = stack.taker_backend().await;
        taker
            .accept_consignment(asset, &change)
            .await
            .expect("taker imports its RGB change");
        taker.sync_wallet().await.expect("taker re-sync after the swap");
        let post_total: u64 = taker
            .inventory(asset)
            .await
            .expect("taker inventory post-sell")
            .iter()
            .map(|u| u.amount)
            .sum();
        assert_eq!(
            post_total,
            pre_total - amount,
            "taker's RGB after a sell should be the change (pre_total − amount sold)"
        );
    }
    txid
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
