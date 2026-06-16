//! End-to-end test for the autonomous rebalance EXECUTOR (`rebalance_once`, the
//! body of `spawn_rebalance_executor_loop`). Unlike `maker_rebalance_ladder`,
//! which drives the planner + `split_pools` by hand, this exercises the REAL
//! orchestration against a `build_runtime` maker: plan → reserve sources → build
//! → broadcast → mark pending → (chain-observer tick) → converge.
//!
//! Asserts the full source lifecycle the loop relies on:
//!   - one pass returns `Split` and leaves the sources NOT yet settled (Reserved
//!     → PendingBitcoinConfirm, excluded from quoting),
//!   - after the split confirms + a chain-observer tick, the sources read `Spent`
//!     (so the loop's in-flight guard clears),
//!   - the ladder converged: the available piece count grew, the RGB total is
//!     preserved, and a second pass returns `Idle`.
//!
//! Run with the regtest stack up + tools installed (see rfq-rgb/tests/cli.rs):
//!   cargo test -p maker-node --test maker_rebalance_executor -- --ignored

use std::time::Duration;

use maker_node::{
    build_runtime, now_ms, rebalance_once, IntervalsConfig, MakerNodeConfig, MakerSection,
    RebalancePolicyConfig, RebalanceTick, RgbConfig, SignerConfig,
};
use rfq_rgb::{test_helpers, RgbBackend};

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see rfq-rgb/tests/cli.rs header"]
async fn executor_pass_splits_reserves_confirms_and_converges() {
    let stack = test_helpers::stack().await;
    let asset = stack.asset();

    // Rebalance enabled, with a ladder small enough that the maker's single
    // bootstrap allocation has a deficit (base 50 → targets [50,25,12]).
    let config = MakerNodeConfig {
        maker: MakerSection {
            node_id: "regtest-rebalance-executor".to_owned(),
            listen_addr: "127.0.0.1:0".to_owned(),
            broker_url: "http://127.0.0.1:3000".to_owned(),
        },
        intervals: IntervalsConfig {
            cleanup: Duration::from_secs(60),
            rebalance: Duration::from_secs(60),
            chain_observer: Duration::from_millis(500),
            strategy: Duration::from_secs(60),
        },
        rebalance: RebalancePolicyConfig {
            enabled: true,
            rgb_ladder_base: 50,
            ladder_ratio: 0.5,
            ladder_tiers: 3,
            ladder_copies: 1,
            min_btc_reserve_sats: 5_000,
            rebalance_max_fee_sats: 10_000,
            ..RebalancePolicyConfig::default()
        },
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

    let runtime = build_runtime(&config).await.expect("build_runtime");
    let maker = runtime.maker;
    let deps = runtime.rebalance_executor.expect("executor deps present");
    let rgb_spec = config.rebalance.rgb_ladder();
    let btc_spec = config.rebalance.btc_ladder();
    let min_reserve = config.rebalance.min_btc_reserve_sats;
    let max_fee = config.rebalance.rebalance_max_fee_sats;

    // Pre-state.
    let pre_total: u64 = deps
        .rgb_backend
        .list_inventory_utxos(&asset)
        .await
        .expect("inventory (pre)")
        .iter()
        .map(|u| u.amount)
        .sum();
    let pre_count = maker.available_rgb_for(&asset).await.len();

    // --- One executor pass: must split ---
    let (_txid, rgb_sources) =
        match rebalance_once(&maker, &deps, &rgb_spec, &btc_spec, min_reserve, max_fee).await {
            RebalanceTick::Split { txid, rgb_sources } => (txid, rgb_sources),
            other => panic!("expected a Split, got {other:?}"),
        };
    assert!(!rgb_sources.is_empty(), "the split consumed RGB sources");

    // ★ In-flight: sources are reserved/pending — NOT settled, and off-limits to
    //   quoting (the loop's guard would skip until they confirm).
    assert!(
        !maker.rgb_sources_settled(&rgb_sources).await,
        "sources must be pending (not Spent) before the split confirms"
    );
    let available_during = maker.available_rgb_for(&asset).await;
    for s in &rgb_sources {
        assert!(
            !available_during.iter().any(|(o, _, _)| o == s),
            "a source mid-split must not be quotable"
        );
    }

    // --- Confirm + one chain-observer tick (sync → ingest → sweep) ---
    stack.confirm().expect("mine + index the rebalance tx");
    deps.rgb_backend.sync_wallet().await.expect("sync wallet");
    let now = now_ms();
    let btc = deps
        .rgb_backend
        .list_btc_only_utxos(&deps.assets, now)
        .await
        .expect("btc-only");
    maker.ingest_btc_change_utxos(btc).await;
    for a in &deps.assets {
        let rgb = deps.rgb_backend.list_inventory_utxos(a).await.expect("rgb inv");
        maker.ingest_rgb_change_utxos(rgb).await;
    }
    maker.sweep_confirmations().await;

    // ★ Sources retired to Spent → the loop's in-flight guard clears.
    assert!(
        maker.rgb_sources_settled(&rgb_sources).await,
        "sources must read Spent after the split confirms"
    );

    // ★ Converged: total preserved, more pieces, second pass idle.
    let post_total: u64 = deps
        .rgb_backend
        .list_inventory_utxos(&asset)
        .await
        .expect("inventory (post)")
        .iter()
        .map(|u| u.amount)
        .sum();
    assert_eq!(post_total, pre_total, "rebalance is a self-transfer; RGB total preserved");
    let post_count = maker.available_rgb_for(&asset).await.len();
    assert!(
        post_count > pre_count,
        "the ladder grew the available piece count ({pre_count} → {post_count})"
    );
    assert!(
        matches!(
            rebalance_once(&maker, &deps, &rgb_spec, &btc_spec, min_reserve, max_fee).await,
            RebalanceTick::Idle
        ),
        "a second pass must be Idle — the ladder converged"
    );
}
