//! Phase-1 spike for the UTXO-rebalancing ladder (`LibRgbBackend::split_asset`).
//!
//! De-risks the open question from [[project_maker_tapret_change_stranding]]:
//! can a self-send split an RGB UTXO into MULTIPLE pieces whose rung outputs sit
//! on the **OUTER (k0)** keychain — which bp-wallet rescans normally — while only
//! the single MPC commitment host stays on the troublesome tweaked k10/0 path?
//! If `list_inventory_utxos` recognizes the k0 rungs after a sync, the unified
//! "one tx splits BTC + every RGB asset" design holds. If it doesn't, RGB rungs
//! must sit on k10 and we fall back to the recovery pattern.
//!
//! Asserts the split is a faithful self-transfer:
//!   - the RGB total is preserved (nothing minted or burned),
//!   - the source allocation is consumed,
//!   - each rung re-lands on the split tx, recognized as inventory, on keychain 0,
//!   - the remainder lands on the pinned host terminal &10/0.
//!
//! Run with the regtest stack up + tools installed (see rfq-rgb/tests/cli.rs):
//!   cargo test -p maker-node --test maker_split_asset_spike -- --ignored

use rfq_btc::{BitcoinClient, ElectrumClient};
use rfq_rgb::{test_helpers, RgbBackend};

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see rfq-rgb/tests/cli.rs header"]
async fn splits_rgb_into_keychain0_rungs() {
    let stack = test_helpers::stack().await;
    let asset = stack.asset();
    let electrum = ElectrumClient::connect(stack.electrum_url()).expect("electrum connect");

    // Pick the fattest RGB allocation to split + snapshot the total.
    let (pre_total, source_op, source_amount) = {
        let maker = stack.maker_backend().await;
        maker.sync_wallet().await.expect("maker sync (pre-split)");
        let inv = maker
            .list_inventory_utxos(&asset)
            .await
            .expect("maker inventory (pre-split)");
        let total: u64 = inv.iter().map(|u| u.amount).sum();
        let fat = inv
            .into_iter()
            .max_by_key(|u| u.amount)
            .expect("maker holds at least one RGB allocation to split");
        (total, fat.outpoint, fat.amount)
    };
    assert!(
        source_amount >= 4,
        "source allocation {source_amount} too small to split into a ladder"
    );

    // Two rungs of a quarter each; the remaining half is the host remainder.
    let rung = source_amount / 4;
    let rungs = vec![rung, rung];
    let expected_remainder = source_amount - 2 * rung;

    // On-chain (value, scriptPubkey) of the source + a BTC-only fee UTXO — exactly
    // what the `maker rebalance` CLI will feed the backend.
    let source_txout = electrum
        .get_outpoint(&source_op)
        .await
        .expect("fetch source output");
    let (raw_tx, split_txid) = {
        let maker = stack.maker_backend().await;
        let fee_utxo = maker
            .list_btc_only_utxos(std::slice::from_ref(&asset), 0)
            .await
            .expect("btc-only utxos")
            .into_iter()
            .max_by_key(|u| u.value_sats)
            .expect("a BTC-only UTXO to fund the fee");
        maker
            .split_asset(
                &asset,
                (
                    source_op.clone(),
                    source_txout.value_sats,
                    source_txout.script_pubkey,
                ),
                (
                    fee_utxo.outpoint.clone(),
                    fee_utxo.value_sats,
                    fee_utxo.script_pubkey.clone(),
                ),
                rungs.clone(),
                1000,
            )
            .await
            .expect("build + sign asset split")
    };

    // Broadcast + confirm (the harness mines).
    let broadcast_txid = stack.broadcast(&raw_tx).expect("broadcast split tx");
    assert_eq!(
        broadcast_txid, split_txid,
        "broadcast txid matches the built witness id"
    );

    // Re-open the wallet, sync, and assert the split landed correctly.
    let maker = stack.maker_backend().await;
    maker.sync_wallet().await.expect("maker sync (post-split)");
    let inv = maker
        .list_inventory_utxos(&asset)
        .await
        .expect("maker inventory (post-split)");
    let post_total: u64 = inv.iter().map(|u| u.amount).sum();

    // ★ Self-transfer: no RGB minted or burned.
    assert_eq!(
        post_total, pre_total,
        "split is a self-transfer; the RGB total must be preserved"
    );
    // ★ The source allocation is consumed.
    assert!(
        !inv.iter().any(|u| u.outpoint == source_op),
        "the source outpoint must be consumed by the split tx"
    );

    // ★ The pieces from the split tx: the 2 rungs + the remainder all re-land here.
    let from_split: Vec<_> = inv
        .iter()
        .filter(|u| u.outpoint.txid == split_txid)
        .collect();
    assert_eq!(
        from_split.len(),
        3,
        "expected 2 rungs + 1 remainder on the split tx, got {}: {from_split:#?}",
        from_split.len()
    );

    // ★ THE SPIKE ASSERTION: each rung-sized piece sits on keychain 0 (rescannable),
    // recognized as inventory. The remainder sits on the pinned host &10/0.
    let mut rung_pieces = 0;
    let mut remainder_pieces = 0;
    for piece in &from_split {
        let terminal = maker
            .debug_outpoint_terminal(&piece.outpoint)
            .await
            .expect("terminal lookup")
            .expect("split piece must derive from the wallet descriptor");
        if piece.amount == rung {
            assert_eq!(
                terminal.0, 0,
                "rung output must land on keychain 0 (rescannable), got terminal {terminal:?}"
            );
            rung_pieces += 1;
        } else if piece.amount == expected_remainder {
            assert_eq!(
                terminal,
                (10, 0),
                "remainder must land on the pinned host terminal &10/0, got {terminal:?}"
            );
            remainder_pieces += 1;
        } else {
            panic!(
                "unexpected piece amount {} (rung={rung}, remainder={expected_remainder})",
                piece.amount
            );
        }
    }
    assert_eq!(rung_pieces, 2, "both rungs recognized on keychain 0");
    assert_eq!(remainder_pieces, 1, "remainder recognized on the pinned host");
}
