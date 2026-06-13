//! Phase-7 e2e: the real ladder planner (`plan_ladder` + `assemble_rebalance_tx`)
//! driving `split_pools` end-to-end, the way the executor loop does — but without
//! reservations (exclusive wallet access, like the other `--ignored` tests).
//!
//! Unlike the Phase-3 combined check (which hard-codes two rungs), this drives
//! the ACTUAL planner against the maker's live inventory with a multi-tier spec,
//! then asserts the pool CONVERGES: after one split the asset's UTXO count meets
//! the ladder target, the RGB total is preserved, and a second `plan_ladder`
//! returns `None` (idempotent — nothing left to split).
//!
//! Coverage note: this exercises the single-contract `Batch { main, extras: [] }`
//! path. The multi-contract MPC-bundle path (`extras` populated) is written +
//! compiles but is NOT yet e2e-tested — the shared regtest harness issues only
//! one contract. Validating ≥2 contracts in one tx needs a second issued contract
//! in `test_helpers::stack()` (tracked as follow-up).
//!
//! Run with the regtest stack up + tools installed (see rfq-rgb/tests/cli.rs):
//!   cargo test -p maker-node --test maker_rebalance_ladder -- --ignored

use rfq_maker::{assemble_rebalance_tx, plan_ladder, AssetSplit, LadderSpec};
use rfq_rgb::{test_helpers, RgbBackend};

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see rfq-rgb/tests/cli.rs header"]
async fn ladder_splits_converge_and_are_idempotent() {
    let stack = test_helpers::stack().await;
    let asset = stack.asset();

    // Snapshot the maker's RGB inventory + size a multi-tier ladder relative to
    // its total so the single fat allocation always has a deficit to fill.
    let (pre_total, pre_values) = {
        let maker = stack.maker_backend().await;
        maker.sync_wallet().await.expect("maker sync (pre)");
        let inv = maker
            .list_inventory_utxos(&asset)
            .await
            .expect("inventory (pre)");
        let total: u64 = inv.iter().map(|u| u.amount).sum();
        let values: Vec<(rfq_types::Outpoint, u64)> =
            inv.iter().map(|u| (u.outpoint.clone(), u.amount)).collect();
        (total, values)
    };
    assert!(
        pre_total >= 8,
        "RGB total {pre_total} too small for a ladder"
    );

    // base = total/8, halving, 2 copies × 3 tiers → up to 6 target rungs.
    let spec = LadderSpec {
        base: (pre_total / 8).max(1),
        ratio: 0.5,
        tiers: 3,
        copies: 2,
        min_piece: 1,
    };
    let target_pieces = spec.target_rungs().len();

    let (source, source_amount, rungs) =
        plan_ladder(&pre_values, &spec).expect("a fat allocation to split into a ladder");
    let source_btc_sats = {
        let maker = stack.maker_backend().await;
        let inv = maker.list_inventory_utxos(&asset).await.expect("inventory");
        inv.iter()
            .find(|u| u.outpoint == source)
            .map(|u| u.btc_sats)
            .expect("source still present")
    };

    // Fattest BTC UTXO funds the anchors + fee (no BTC rungs this test).
    let btc_source = {
        let maker = stack.maker_backend().await;
        maker
            .list_btc_only_utxos(std::slice::from_ref(&asset), 0)
            .await
            .expect("btc-only")
            .into_iter()
            .max_by_key(|u| u.value_sats)
            .map(|u| (u.outpoint, u.value_sats))
            .expect("a BTC-only UTXO to fund the rebalance")
    };

    // Assemble the plan exactly as the executor would (budget against the BTC
    // source value; no BTC laddering).
    let plan = assemble_rebalance_tx(
        vec![AssetSplit {
            asset: asset.clone(),
            source: source.clone(),
            source_amount,
            source_btc_sats,
            rungs: rungs.clone(),
        }],
        None,
        btc_source.1,
        1000,
    )
    .expect("a fundable plan");
    let planned_rungs = plan.assets[0].rungs.len();

    // Build + broadcast the single split tx (BTC source as fee funder).
    let (raw_tx, split_txid) = {
        let maker = stack.maker_backend().await;
        maker
            .split_pools(
                plan.assets
                    .iter()
                    .map(|a| (a.asset.clone(), a.source.clone(), a.rungs.clone()))
                    .collect(),
                Some((btc_source.0.clone(), Vec::new())),
                plan.fee_sats,
            )
            .await
            .expect("build + sign ladder split")
    };
    let broadcast_txid = stack.broadcast(&raw_tx).expect("broadcast ladder split");
    assert_eq!(
        broadcast_txid, split_txid,
        "broadcast txid matches built witness id"
    );

    // Re-sync and assert convergence.
    let maker = stack.maker_backend().await;
    maker.sync_wallet().await.expect("maker sync (post)");
    let inv = maker
        .list_inventory_utxos(&asset)
        .await
        .expect("inventory (post)");
    let post_total: u64 = inv.iter().map(|u| u.amount).sum();

    // ★ Self-transfer: total preserved.
    assert_eq!(
        post_total, pre_total,
        "ladder split is a self-transfer; RGB total preserved"
    );
    // ★ Source consumed.
    assert!(
        !inv.iter().any(|u| u.outpoint == source),
        "the split source must be consumed"
    );
    // ★ The split produced (planned rungs + 1 remainder) recognized pieces.
    let from_split = inv.iter().filter(|u| u.outpoint.txid == split_txid).count();
    assert_eq!(
        from_split,
        planned_rungs + 1,
        "expected {planned_rungs} rungs + 1 remainder on the split tx, got {from_split}"
    );
    // ★ Enough granularity for the ladder target (and the default min_utxo_count of 3).
    assert!(
        inv.len() >= target_pieces.min(planned_rungs + 1),
        "post-split inventory {} should cover the ladder",
        inv.len()
    );

    // ★ Idempotent: the planner sees nothing left to split.
    let post_values: Vec<(rfq_types::Outpoint, u64)> =
        inv.iter().map(|u| (u.outpoint.clone(), u.amount)).collect();
    assert!(
        plan_ladder(&post_values, &spec).is_none(),
        "ladder converged — a second plan_ladder must return None, got {:?}",
        plan_ladder(&post_values, &spec)
    );
}
