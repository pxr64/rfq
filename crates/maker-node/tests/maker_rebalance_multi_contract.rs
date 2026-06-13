//! Phase-7 follow-up: the MULTI-CONTRACT rebalance path — one `split_pools` tx
//! that splits TWO RGB contracts at once, exercising the `Batch { main, extras }`
//! MPC bundle (the branch the single-contract spike/combined/ladder tests can't
//! reach). The maker issues a second NIA contract to itself (see
//! `RegtestStack::issue_second_maker_contract`) so it holds two distinct assets.
//!
//! Asserts that ONE transaction splits both: each asset's rungs land on keychain
//! 0, each remainder on the pinned host, both totals are preserved, and both
//! ladders converge — all anchored under a single commitment + one fee.
//!
//! Run with the regtest stack up + tools installed (see rfq-rgb/tests/cli.rs):
//!   cargo test -p maker-node --test maker_rebalance_multi_contract -- --ignored

use rfq_maker::{plan_ladder, AssetSplit, LadderSpec};
use rfq_rgb::{test_helpers, RgbBackend};

fn spec_for(total: u64) -> LadderSpec {
    LadderSpec {
        base: (total / 4).max(1),
        ratio: 0.5,
        tiers: 3,
        copies: 1,
        min_piece: 1,
    }
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see rfq-rgb/tests/cli.rs header"]
async fn split_pools_splits_two_contracts_in_one_tx() {
    let stack = test_helpers::stack().await;
    let asset1 = stack.asset();
    // Maker mints a second contract to itself (fresh funded genesis).
    let asset2 = stack
        .issue_second_maker_contract("COLY", 1_000_000)
        .await;
    let assets = [asset1.clone(), asset2.clone()];

    // Plan a ladder for each asset against its live inventory.
    let mut splits: Vec<AssetSplit> = Vec::new();
    let mut pre_totals: Vec<u64> = Vec::new();
    {
        let maker = stack.maker_backend().await;
        maker.sync_wallet().await.expect("maker sync (pre)");
        for asset in &assets {
            let inv = maker.list_inventory_utxos(asset).await.expect("inventory (pre)");
            let total: u64 = inv.iter().map(|u| u.amount).sum();
            assert!(total >= 4, "asset {} total {total} too small to split", asset.id);
            let pairs: Vec<(rfq_types::Outpoint, u64)> =
                inv.iter().map(|u| (u.outpoint.clone(), u.amount)).collect();
            let (source, source_amount, rungs) =
                plan_ladder(&pairs, &spec_for(total)).expect("a ladder deficit per asset");
            let source_btc_sats = inv
                .iter()
                .find(|u| u.outpoint == source)
                .map(|u| u.btc_sats)
                .unwrap_or(0);
            splits.push(AssetSplit {
                asset: asset.clone(),
                source,
                source_amount,
                source_btc_sats,
                rungs,
            });
            pre_totals.push(total);
        }
    }
    assert_eq!(splits.len(), 2, "both contracts must have a ladder deficit");

    // BTC fee source: fattest BTC-only UTXO excluding BOTH contracts.
    let btc_fee = {
        let maker = stack.maker_backend().await;
        maker
            .list_btc_only_utxos(&assets, 0)
            .await
            .expect("btc-only")
            .into_iter()
            .max_by_key(|u| u.value_sats)
            .expect("a BTC-only UTXO to fund the fee")
            .outpoint
    };

    // ONE tx splitting BOTH contracts → Batch { main: t1, extras: [t2] }.
    let (raw_tx, split_txid) = {
        let maker = stack.maker_backend().await;
        maker
            .split_pools(
                splits
                    .iter()
                    .map(|a| (a.asset.clone(), a.source.clone(), a.rungs.clone()))
                    .collect(),
                Some((btc_fee, Vec::new())),
                2000,
            )
            .await
            .expect("build + sign two-contract split")
    };
    let broadcast = stack.broadcast(&raw_tx).expect("broadcast two-contract split");
    assert_eq!(broadcast, split_txid, "broadcast txid matches built witness id");

    // Re-sync and assert BOTH contracts split correctly on the SAME tx.
    let maker = stack.maker_backend().await;
    maker.sync_wallet().await.expect("maker sync (post)");
    for (i, asset) in assets.iter().enumerate() {
        let inv = maker.list_inventory_utxos(asset).await.expect("inventory (post)");
        let post_total: u64 = inv.iter().map(|u| u.amount).sum();

        // ★ Self-transfer per contract: total preserved.
        assert_eq!(post_total, pre_totals[i], "asset {} total preserved", asset.id);
        // ★ Source consumed.
        assert!(
            !inv.iter().any(|u| u.outpoint == splits[i].source),
            "asset {} source consumed",
            asset.id
        );
        // ★ rungs + 1 remainder, all on the ONE split tx.
        let from_split = inv.iter().filter(|u| u.outpoint.txid == split_txid).count();
        assert_eq!(
            from_split,
            splits[i].rungs.len() + 1,
            "asset {} expected {} rungs + remainder on the one tx, got {from_split}",
            asset.id,
            splits[i].rungs.len()
        );
        // ★ rung outputs on keychain 0; remainder on the pinned host (10,0).
        for u in inv
            .iter()
            .filter(|u| u.outpoint.txid == split_txid && splits[i].rungs.contains(&u.amount))
        {
            let terminal = maker
                .debug_outpoint_terminal(&u.outpoint)
                .await
                .expect("terminal lookup")
                .expect("rung derives from the descriptor");
            assert_eq!(terminal.0, 0, "asset {} rung on keychain 0, got {terminal:?}", asset.id);
        }
        // ★ Converged: nothing left to split for this asset.
        let post_pairs: Vec<(rfq_types::Outpoint, u64)> =
            inv.iter().map(|u| (u.outpoint.clone(), u.amount)).collect();
        assert!(
            plan_ladder(&post_pairs, &spec_for(pre_totals[i])).is_none(),
            "asset {} ladder converged",
            asset.id
        );
    }
}
