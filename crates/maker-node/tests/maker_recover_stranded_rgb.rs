//! End-to-end test for the RGB recovery sweep (`LibRgbBackend::recover_stranded_rgb`).
//!
//! Recovery re-homes RGB the wallet can't see (tapret host outputs bp-wallet
//! never tracked) by SPENDING the outpoint directly — it sources each input's
//! `(value, scriptPubkey)` from the chain and its signing terminal from the
//! descriptor, never consulting `wallet.utxos()`. So sweeping ANY maker RGB
//! output exercises the exact recovery code path; this test sweeps a normal
//! tracked allocation (creating a genuinely stranded one deterministically isn't
//! feasible now that the pinning fix prevents stranding).
//!
//! Asserts the sweep is a faithful self-transfer:
//!   - the RGB total is preserved (nothing minted or burned),
//!   - the swept amount re-lands on the recovery tx, recognized as inventory,
//!   - on the PINNED host terminal &10/0 (so it can never re-strand).
//!
//! Run with the regtest stack up + tools installed (see rfq-rgb/tests/cli.rs):
//!   cargo test -p maker-node --test maker_recover_stranded_rgb -- --ignored

use rfq_btc::{BitcoinClient, ElectrumClient};
use rfq_rgb::{test_helpers, RgbBackend};

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see rfq-rgb/tests/cli.rs header"]
async fn recovers_rgb_into_pinned_host() {
    let stack = test_helpers::stack().await;
    let asset = stack.asset();
    let electrum = ElectrumClient::connect(stack.electrum_url()).expect("electrum connect");

    // Pick an RGB allocation to sweep + snapshot the total.
    let (pre_total, sweep_op) = {
        let maker = stack.maker_backend().await;
        maker.sync_wallet().await.expect("maker sync (pre-recovery)");
        let inv = maker
            .list_inventory_utxos(&asset)
            .await
            .expect("maker inventory (pre-recovery)");
        let total: u64 = inv.iter().map(|u| u.amount).sum();
        let op = inv
            .first()
            .expect("maker holds at least one RGB allocation to sweep")
            .outpoint
            .clone();
        (total, op)
    };
    let swept_amount = {
        let maker = stack.maker_backend().await;
        let inv = maker.list_inventory_utxos(&asset).await.expect("inventory");
        inv.iter()
            .find(|u| u.outpoint == sweep_op)
            .expect("swept outpoint still present")
            .amount
    };

    // On-chain value + scriptPubkey of the output to sweep, and a BTC-only UTXO
    // to fund the fee — exactly what the `maker recover` CLI feeds the backend.
    let txout = electrum
        .get_outpoint(&sweep_op)
        .await
        .expect("fetch swept output");
    let (raw_tx, recovery_txid, swept) = {
        let maker = stack.maker_backend().await;
        let fee_utxo = maker
            .list_btc_only_utxos(&asset, 0)
            .await
            .expect("btc-only utxos")
            .into_iter()
            .max_by_key(|u| u.value_sats)
            .expect("a BTC-only UTXO to fund the fee");
        maker
            .recover_stranded_rgb(
                &asset,
                vec![(sweep_op.clone(), txout.value_sats, txout.script_pubkey)],
                (
                    fee_utxo.outpoint.clone(),
                    fee_utxo.value_sats,
                    fee_utxo.script_pubkey.clone(),
                ),
                1000,
            )
            .await
            .expect("build + sign recovery sweep")
    };
    assert_eq!(swept, vec![sweep_op.clone()], "the swept outpoint is reported");

    // Broadcast + confirm (the harness mines).
    let broadcast_txid = stack.broadcast(&raw_tx).expect("broadcast recovery tx");
    assert_eq!(
        broadcast_txid, recovery_txid,
        "broadcast txid matches the built witness id"
    );

    // Re-open the wallet, sync, and assert the recovery landed correctly.
    let maker = stack.maker_backend().await;
    maker.sync_wallet().await.expect("maker sync (post-recovery)");
    let inv = maker
        .list_inventory_utxos(&asset)
        .await
        .expect("maker inventory (post-recovery)");
    let post_total: u64 = inv.iter().map(|u| u.amount).sum();

    // ★ Self-transfer: no RGB minted or burned.
    assert_eq!(
        post_total, pre_total,
        "recovery is a self-transfer; the RGB total must be preserved"
    );
    // ★ The swept allocation no longer sits on the old outpoint...
    assert!(
        !inv.iter().any(|u| u.outpoint == sweep_op),
        "the swept outpoint must be consumed by the recovery tx"
    );
    // ★ ...it re-lands on the recovery tx, recognized as inventory...
    let recovered = inv
        .iter()
        .find(|u| u.outpoint.txid == recovery_txid)
        .expect("the swept RGB must re-land on the recovery tx and be recognized");
    assert_eq!(
        recovered.amount, swept_amount,
        "the recovered allocation carries the full swept amount"
    );
    // ★ ...on the PINNED host terminal &10/0, so it can never re-strand.
    let terminal = maker
        .debug_outpoint_terminal(&recovered.outpoint)
        .await
        .expect("terminal lookup");
    assert_eq!(
        terminal,
        Some((10, 0)),
        "recovered RGB must land on the pinned host terminal &10/0 (got {terminal:?})"
    );
}
