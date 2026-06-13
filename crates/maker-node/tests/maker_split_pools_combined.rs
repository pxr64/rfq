//! Phase-3 check for the combined rebalance primitive
//! (`LibRgbBackend::split_pools` → `swap::build_rebalance_tx`).
//!
//! Validates the NEW behaviour beyond the single-contract spike: folding BTC
//! rungs into the SAME transaction that splits an RGB asset, under one tapret
//! commitment + one fee. One asset (the stack issues a single contract; the
//! 2-contract MPC-bundle path is exercised by the Phase-7 ladder e2e once the
//! harness can issue a second contract).
//!
//! Asserts:
//!   - RGB total preserved; the asset's rungs land on keychain 0, remainder on
//!     the pinned host &10/0 — all recognized as inventory,
//!   - the BTC rungs appear as fresh BTC-only UTXOs of the requested sizes.
//!
//! Run with the regtest stack up + tools installed (see rfq-rgb/tests/cli.rs):
//!   cargo test -p maker-node --test maker_split_pools_combined -- --ignored

use rfq_rgb::{test_helpers, RgbBackend};

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see rfq-rgb/tests/cli.rs header"]
async fn split_pools_splits_rgb_and_btc_in_one_tx() {
    let stack = test_helpers::stack().await;
    let asset = stack.asset();

    const BTC_RUNGS: [u64; 2] = [10_000, 5_000];

    // Snapshot RGB total + pick the fattest allocation to split.
    let (pre_total, rgb_source, rgb_amount) = {
        let maker = stack.maker_backend().await;
        maker.sync_wallet().await.expect("maker sync (pre)");
        let inv = maker.list_inventory_utxos(&asset).await.expect("inventory (pre)");
        let total: u64 = inv.iter().map(|u| u.amount).sum();
        let fat = inv
            .into_iter()
            .max_by_key(|u| u.amount)
            .expect("an RGB allocation to split");
        (total, fat.outpoint, fat.amount)
    };
    assert!(rgb_amount >= 4, "RGB source {rgb_amount} too small to split");
    let rung = rgb_amount / 4;
    let rgb_rungs = vec![rung, rung];

    // A fat BTC-only UTXO to carve the BTC rungs (and fund anchors + fee) from,
    // plus the pre-split count to assert growth.
    let (btc_source, pre_btc_count) = {
        let maker = stack.maker_backend().await;
        let btc = maker
            .list_btc_only_utxos(std::slice::from_ref(&asset), 0)
            .await
            .expect("btc-only (pre)");
        let fat = btc
            .iter()
            .max_by_key(|u| u.value_sats)
            .expect("a BTC-only UTXO to split");
        assert!(
            fat.value_sats > BTC_RUNGS.iter().sum::<u64>() + 10_000,
            "btc source {} too small for rungs + fee",
            fat.value_sats
        );
        (fat.outpoint.clone(), btc.len())
    };

    let (raw_tx, split_txid) = {
        let maker = stack.maker_backend().await;
        maker
            .split_pools(
                vec![(asset.clone(), rgb_source.clone(), rgb_rungs.clone())],
                Some((btc_source.clone(), BTC_RUNGS.to_vec())),
                1000,
            )
            .await
            .expect("build + sign combined split")
    };

    let broadcast_txid = stack.broadcast(&raw_tx).expect("broadcast combined split");
    assert_eq!(broadcast_txid, split_txid, "broadcast txid matches built witness id");

    // Re-sync and assert both pools landed.
    let maker = stack.maker_backend().await;
    maker.sync_wallet().await.expect("maker sync (post)");

    // --- RGB side ---
    let inv = maker.list_inventory_utxos(&asset).await.expect("inventory (post)");
    let post_total: u64 = inv.iter().map(|u| u.amount).sum();
    assert_eq!(post_total, pre_total, "RGB total preserved (self-transfer)");
    assert!(
        !inv.iter().any(|u| u.outpoint == rgb_source),
        "RGB source consumed"
    );
    let from_split: Vec<_> = inv.iter().filter(|u| u.outpoint.txid == split_txid).collect();
    let remainder = rgb_amount - 2 * rung;
    let mut rungs_on_k0 = 0;
    let mut remainder_on_host = 0;
    for piece in &from_split {
        let terminal = maker
            .debug_outpoint_terminal(&piece.outpoint)
            .await
            .expect("terminal lookup")
            .expect("split piece derives from the descriptor");
        if piece.amount == rung {
            assert_eq!(terminal.0, 0, "RGB rung on keychain 0, got {terminal:?}");
            rungs_on_k0 += 1;
        } else if piece.amount == remainder {
            assert_eq!(terminal, (10, 0), "remainder on pinned host, got {terminal:?}");
            remainder_on_host += 1;
        }
    }
    assert_eq!(rungs_on_k0, 2, "both RGB rungs recognized on keychain 0");
    assert_eq!(remainder_on_host, 1, "RGB remainder recognized on the host");

    // --- BTC side ---
    let btc = maker
        .list_btc_only_utxos(std::slice::from_ref(&asset), 0)
        .await
        .expect("btc-only (post)");
    for want in BTC_RUNGS {
        assert!(
            btc.iter()
                .any(|u| u.outpoint.txid == split_txid && u.value_sats == want),
            "expected a {want}-sat BTC rung from the split tx; got {:#?}",
            btc.iter()
                .filter(|u| u.outpoint.txid == split_txid)
                .collect::<Vec<_>>()
        );
    }
    assert!(
        btc.len() > pre_btc_count,
        "BTC-only UTXO count should grow ({pre_btc_count} → {})",
        btc.len()
    );
}
