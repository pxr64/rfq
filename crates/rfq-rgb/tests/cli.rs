//! Integration tests for [`LibRgbBackend`] against a live regtest stack.
//!
//! All `#[ignore]`d tests run on top of the self-bootstrapping harness in
//! [`common::stack`] (issue #23). One-time setup:
//!
//! ```bash
//! make -C infra/regtest regtest-up
//! make -C infra/regtest rgb-tools-install
//! cargo test -p rfq-rgb -- --ignored
//! ```
//!
//! No `RGB_*` env vars. The harness creates wallets, funds them, issues the
//! NIA contract, and runs an issuer→maker transfer — all inside a per-process
//! tempdir that auto-cleans on exit.

use std::path::PathBuf;

use rfq_rgb::{ConsignmentInfo, LibRgbBackend, RgbBackend, RgbError, TxOut};
use rfq_types::Outpoint;

mod common;

/// Pure parse test — no live stack required. NOT `#[ignore]`; runs as part of
/// the default test suite.
#[tokio::test]
async fn validate_invoice_rejects_garbage() {
    let backend = LibRgbBackend::new(
        PathBuf::from("/tmp/nonexistent-stash"),
        "irrelevant".to_owned(),
        "regtest".to_owned(),
        "localhost:50001".to_owned(),
        PathBuf::from("/tmp/nonexistent-signer"),
        String::new(),
    );

    assert!(matches!(
        backend.validate_invoice("not an rgb invoice").await,
        Err(RgbError::InvalidInvoice)
    ));
    assert!(matches!(
        backend.validate_invoice("").await,
        Err(RgbError::InvalidInvoice)
    ));
    assert!(matches!(
        backend.validate_invoice("rgb:malformed").await,
        Err(RgbError::InvalidInvoice)
    ));
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn list_inventory_utxos_returns_per_utxo_outpoints() {
    let stack = common::stack().await;
    let asset = stack.asset();

    let backend = stack.maker_backend().await;
    let utxos = backend
        .list_inventory_utxos(&asset)
        .await
        .expect("list_inventory_utxos should succeed against the bootstrapped stash");

    assert!(
        !utxos.is_empty(),
        "expected at least one UTXO; bootstrap should have transferred 1000 units to the maker"
    );

    let total: u64 = utxos.iter().map(|u| u.amount).sum();
    assert!(total > 0, "expected positive total amount; got {total}");

    let zero_txid = "0".repeat(64);
    dbg!(&utxos);
    for utxo in &utxos {
        assert_eq!(utxo.asset_id, asset);
        assert_eq!(
            utxo.outpoint.txid.len(),
            64,
            "expected 64-hex-char txid, got {:?}",
            utxo.outpoint.txid
        );
        assert_ne!(
            utxo.outpoint.txid, zero_txid,
            "outpoint txid should not be all-zeros; LibRgbBackend should surface real seal data"
        );
    }
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn create_invoice_returns_rgb_invoice_referencing_the_contract() {
    let stack = common::stack().await;
    let asset = stack.asset();

    let backend = stack.maker_backend().await;
    let invoice = backend
        .create_invoice(&asset, 100)
        .await
        .expect("create_invoice should succeed against the bootstrapped wallet");

    assert!(
        invoice.starts_with("rgb:"),
        "expected an rgb: invoice, got `{invoice}`"
    );
    // The contract id is BAID-encoded inside the invoice; the substring check
    // is a coarse smoke test that the binding actually happened.
    assert!(
        invoice.contains(&asset.id) || invoice.contains(&asset.id[4..]),
        "invoice should reference the contract id; got `{invoice}`"
    );
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn validate_incoming_consignment_accepts_a_real_consignment() {
    use base64::Engine as _;

    let stack = common::stack().await;
    let consignment_b64 =
        base64::engine::general_purpose::STANDARD.encode(stack.consignment_bytes());

    let backend = stack.maker_backend().await;
    let info = backend
        .validate_incoming_consignment(&consignment_b64, stack.maker_invoice())
        .await
        .expect("validate_incoming_consignment should accept the bootstrap transfer");

    assert!(
        info.total_amount > 0,
        "expected positive total_amount; got {}",
        info.total_amount,
    );
    assert!(
        !info.outpoints.is_empty(),
        "expected at least one outpoint extracted from the validated transfer",
    );
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn create_swap_psbt_buy_produces_psbt_and_consignment() {
    let stack = common::stack().await;
    let asset = stack.asset();
    let backend = stack.maker_backend().await;

    // RGB composition smoke test: the maker mints an invoice against its own
    // contract, the maker's own RGB inventory provides the inputs, taker BTC
    // inputs are empty (BTC side is exercised at the rfq-maker layer). This
    // asserts the RGB transition build + embed + commit + transfer +
    // maker-sign path yields a stable witness txid.
    let invoice = backend
        .create_invoice(&asset, 100)
        .await
        .expect("create_invoice");
    let utxos = backend
        .list_inventory_utxos(&asset)
        .await
        .expect("list_inventory_utxos");
    let maker_outpoints: Vec<_> = utxos.iter().map(|u| u.outpoint.clone()).collect();
    assert!(
        !maker_outpoints.is_empty(),
        "maker needs at least one RGB allocation to spend"
    );

    let transfer = backend
        .create_swap_psbt_buy(&invoice, 100, &maker_outpoints, &[], "bcrt1qtaker", 1_000, 100)
        .await
        .expect("create_swap_psbt_buy should compose a swap PSBT");

    assert!(
        !transfer.partial_psbt.is_empty(),
        "expected a non-empty base64 PSBT"
    );
    assert!(
        transfer.consignment.is_some(),
        "buy side emits the maker's consignment"
    );
    let wt = transfer
        .expected_witness_txid
        .expect("declared-funding buy commits a stable witness txid");
    assert_eq!(wt.len(), 64, "witness txid should be 64 hex chars, got {wt:?}");
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn create_swap_psbt_sell_produces_psbt() {
    let stack = common::stack().await;
    let asset = stack.asset();
    let backend = stack.maker_backend().await;

    // Build `ConsignmentInfo` synthetically from the maker's actual
    // inventory: the bootstrap put 1000 units at a maker keychain-9 outpoint,
    // and `list_inventory_utxos` surfaces it. Using these (rather than
    // `validate_incoming_consignment` output) keeps sum-of-spendable ==
    // deliver_amount, which is what `build_sell_transition` requires. A real
    // taker→maker round-trip with chain-correct prevouts is B5 work.
    let utxos = backend
        .list_inventory_utxos(&asset)
        .await
        .expect("list_inventory_utxos");
    let total_amount: u64 = utxos.iter().map(|u| u.amount).sum();
    let outpoints: Vec<Outpoint> = utxos.iter().map(|u| u.outpoint.clone()).collect();
    assert!(
        total_amount > 0 && !outpoints.is_empty(),
        "bootstrap should have transferred RGB to the maker"
    );
    let info = ConsignmentInfo {
        total_amount,
        outpoints: outpoints.clone(),
    };

    // Fresh maker invoice for the swap delivery, round-tripped into the typed
    // components rfq-maker passes at /sign time.
    let maker_invoice = backend
        .create_invoice(&asset, total_amount)
        .await
        .expect("create_invoice");
    let parts = backend
        .parse_maker_invoice(&maker_invoice)
        .await
        .expect("parse_maker_invoice");
    assert_eq!(parts.amount, Some(total_amount));

    // Synthesize taker_rgb_prevouts — cli.rs has no electrum access to fetch
    // real prevouts; composition only needs the outpoint at PSBT-input time.
    let taker_rgb_prevouts: Vec<(Outpoint, TxOut)> = outpoints
        .iter()
        .map(|o| {
            (
                o.clone(),
                TxOut {
                    value_sats: 1_000,
                    script_pubkey: vec![0u8; 22],
                },
            )
        })
        .collect();

    let maker_btc_outpoint = backend.spare_btc_outpoint().await;

    let transfer = backend
        .create_swap_psbt_sell(
            &info,
            &taker_rgb_prevouts,
            &[(
                maker_btc_outpoint,
                TxOut {
                    value_sats: 0,
                    script_pubkey: vec![],
                },
            )],
            parts.contract_id,
            parts.seal,
            total_amount,
            stack.taker_payout_addr(),
            None,
            10_000,
            500,
        )
        .await
        .expect("create_swap_psbt_sell should compose a swap PSBT");

    assert!(
        !transfer.partial_psbt.is_empty(),
        "expected a non-empty base64 PSBT"
    );
    assert!(
        transfer.consignment.is_none(),
        "sell side: taker built its own consignment"
    );
    let wt = transfer
        .expected_witness_txid
        .expect("declared-funding sell commits a stable witness txid");
    assert_eq!(wt.len(), 64, "witness txid should be 64 hex chars, got {wt:?}");
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn finalize_after_taker_sign_returns_extractable_witness_tx() {
    let stack = common::stack().await;
    let asset = stack.asset();
    let backend = stack.maker_backend().await;

    // Reuse the buy composition with no taker BTC inputs: the PSBT then has
    // only maker inputs, so the maker's own descriptor finalizes everything
    // and finalize_after_taker_sign can be exercised end-to-end without a
    // second-party signer. A real two-backend round-trip is B5 territory.
    let invoice = backend
        .create_invoice(&asset, 100)
        .await
        .expect("create_invoice");
    let utxos = backend
        .list_inventory_utxos(&asset)
        .await
        .expect("list_inventory_utxos");
    let maker_outpoints: Vec<Outpoint> = utxos.iter().map(|u| u.outpoint.clone()).collect();
    let transfer = backend
        .create_swap_psbt_buy(&invoice, 100, &maker_outpoints, &[], "bcrt1qtaker", 1_000, 100)
        .await
        .expect("create_swap_psbt_buy should compose a swap PSBT");

    let consignment = transfer.consignment.expect("buy emits consignment");
    let expected_wt = transfer
        .expected_witness_txid
        .clone()
        .expect("declared-funding buy commits a stable witness txid");

    let finalized = backend
        .finalize_after_taker_sign(&transfer.partial_psbt, &consignment)
        .await
        .expect("finalize should succeed when the maker is the sole signer");

    assert_eq!(
        finalized.witness_txid, expected_wt,
        "finalize must surface the same witness id committed at PSBT-build (D3 invariant)"
    );
    assert!(
        !finalized.raw_tx.is_empty(),
        "extracted witness tx should be non-empty"
    );
    assert_eq!(
        finalized.final_consignment_base64, consignment,
        "finalize echoes back the original consignment; no re-emit"
    );
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn finalize_after_taker_sign_rejects_garbage_psbt() {
    let stack = common::stack().await;
    let backend = stack.maker_backend().await;

    // Garbage base64 must surface as FinalizeFailed, not panic.
    let result = backend
        .finalize_after_taker_sign("not-base64!", "Y29uc2lnbm1lbnQ=")
        .await;
    assert!(
        matches!(result, Err(RgbError::FinalizeFailed(_))),
        "expected FinalizeFailed on garbage input, got {result:?}"
    );
}
