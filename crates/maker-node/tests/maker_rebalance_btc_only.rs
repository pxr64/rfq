//! Phase-7 follow-up: the BTC-ONLY rebalance path (`split_pools` with no RGB
//! assets → `swap::build_btc_split`). The combined/ladder tests always carry an
//! RGB transition; this exercises the plain multi-output BTC self-send branch.
//!
//! Uses explicit rungs (not `plan_ladder`) so it's deterministic regardless of
//! how many fat BTC UTXOs the bootstrap left — the point is to validate that
//! `build_btc_split` produces a valid multi-output tx whose pieces land as
//! spendable BTC UTXOs of the requested sizes, with change above the 1000-sat
//! floor.
//!
//! Run with the regtest stack up + tools installed (see rfq-rgb/tests/cli.rs):
//!   cargo test -p maker-node --test maker_rebalance_btc_only -- --ignored

use rfq_rgb::test_helpers;

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see rfq-rgb/tests/cli.rs header"]
async fn split_pools_btc_only_splits_into_pieces() {
    let stack = test_helpers::stack().await;
    let asset = stack.asset();

    // Pick the fattest BTC-only UTXO + snapshot the count.
    let (source, source_sats, pre_count) = {
        let maker = stack.maker_backend().await;
        maker.sync_wallet().await.expect("maker sync (pre)");
        let btc = maker
            .list_btc_only_utxos(std::slice::from_ref(&asset), 0)
            .await
            .expect("btc-only (pre)");
        let fat = btc
            .iter()
            .max_by_key(|u| u.value_sats)
            .expect("a BTC-only UTXO to split");
        (fat.outpoint.clone(), fat.value_sats, btc.len())
    };
    // Two rungs that comfortably fit, leaving change well above the 1000 floor.
    let rungs = vec![source_sats / 4, source_sats / 8];
    assert!(rungs[1] > 1000, "source {source_sats} too small for this test");

    // No RGB assets → build_btc_split.
    let (raw_tx, split_txid) = {
        let maker = stack.maker_backend().await;
        maker
            .split_pools(Vec::new(), Some((source.clone(), rungs.clone())), 1000)
            .await
            .expect("build + sign btc-only split")
    };
    let broadcast = stack.broadcast(&raw_tx).expect("broadcast btc split");
    assert_eq!(broadcast, split_txid, "broadcast txid matches built witness id");

    let maker = stack.maker_backend().await;
    maker.sync_wallet().await.expect("maker sync (post)");
    let btc = maker
        .list_btc_only_utxos(std::slice::from_ref(&asset), 0)
        .await
        .expect("btc-only (post)");

    // ★ Source consumed.
    assert!(
        !btc.iter().any(|u| u.outpoint == source),
        "the split source must be consumed"
    );
    // ★ Each requested rung lands as a spendable BTC UTXO of the exact size.
    for want in &rungs {
        assert!(
            btc.iter()
                .any(|u| u.outpoint.txid == split_txid && u.value_sats == *want),
            "expected a {want}-sat BTC piece from the split; got {:#?}",
            btc.iter()
                .filter(|u| u.outpoint.txid == split_txid)
                .map(|u| u.value_sats)
                .collect::<Vec<_>>()
        );
    }
    // ★ Plus a change output (≥ floor) — 2 rungs + 1 change on the split tx.
    let from_split = btc.iter().filter(|u| u.outpoint.txid == split_txid).count();
    assert_eq!(from_split, 3, "expected 2 rungs + 1 change on the split tx");
    assert!(btc.len() > pre_count, "BTC-only count should grow");
}
