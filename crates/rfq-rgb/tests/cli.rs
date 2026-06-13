//! Integration tests for [`LibRgbBackend`] against a live regtest stack.
//!
//! All `#[ignore]`d tests run on top of the self-bootstrapping harness in
//! [`common::stack`] (issue #23). One-time setup:
//!
//! ```bash
//! make -C infra/regtest regtest-up
//! make -C infra/regtest rgb-tools-install
//! cargo test -p rfq-rgb -- --ignored --test-threads=1
//! ```
//!
//! Run with `--test-threads=1`: tests that shell out to `rgb` against the shared
//! taker/maker stash (the consignment + witness-vout-invoice round-trips) bypass
//! the in-process backend mutex, so concurrent runs can race a half-written stash.
//!
//! No `RGB_*` env vars. The harness creates wallets, funds them, issues the
//! NIA contract, and runs an issuer→maker transfer — all inside a per-process
//! tempdir that auto-cleans on exit.

use std::path::PathBuf;

use rfq_rgb::{ConsignmentInfo, LibRgbBackend, RgbBackend, RgbError, TxOut};
use rfq_types::Outpoint;

// Reuses the regtest harness now living in the lib (gated by
// `cfg(any(test, feature = "test-helpers"))`). The `common` alias keeps
// every existing call site below stable after the tests/common → lib
// move.
use rfq_rgb::test_helpers as common;

/// Pure parse test — no live stack required. NOT `#[ignore]`; runs as part of
/// the default test suite.
#[tokio::test]
async fn validate_invoice_rejects_garbage() {
    let backend = LibRgbBackend::new(
        PathBuf::from("/tmp/nonexistent-stash"),
        "irrelevant".to_owned(),
        "regtest".to_owned(),
        "localhost:60001".to_owned(),
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
    // TODO(provenance): pass the taker's consigned outpoints once this e2e fixture
    // is updated to the provenance model (see docs/provenance-consignment-proposal.md).
    let info = backend
        .validate_incoming_consignment(&consignment_b64, stack.contract_id(), &[])
        .await
        .expect("validate_incoming_consignment should accept the bootstrap transfer");

    assert!(
        info.total_amount > 0,
        "expected positive total_amount; got {}",
        info.total_amount,
    );
    assert!(
        !info.outpoints.is_empty(),
        "expected at least one input outpoint extracted from the validated transfer",
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
        .create_swap_psbt_buy(
            &invoice,
            100,
            &maker_outpoints,
            &[],
            "bcrt1qtaker",
            1_000,
            100,
            &[],
        )
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
    assert_eq!(
        wt.len(),
        64,
        "witness txid should be 64 hex chars, got {wt:?}"
    );
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn create_swap_psbt_sell_produces_psbt() {
    let stack = common::stack().await;
    let asset = stack.asset();
    let backend = stack.maker_backend().await;

    // Build `ConsignmentInfo` synthetically from the maker's actual
    // inventory: the bootstrap put 1000 units at a maker keychain-10 outpoint,
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
            total_amount,
            stack.taker_payout_addr(),
            None,
            10_000,
            500,
            &[],
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
    assert_eq!(
        wt.len(),
        64,
        "witness txid should be 64 hex chars, got {wt:?}"
    );
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
        .create_swap_psbt_buy(
            &invoice,
            100,
            &maker_outpoints,
            &[],
            "bcrt1qtaker",
            1_000,
            100,
            &[],
        )
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

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn buy_round_trip_two_backends_broadcasts() {
    // B5 happy path: drive the full buy flow across two cooperating
    // backends (maker + taker), then broadcast the assembled witness tx
    // through bitcoind. Proves PSBT serialization survives the round
    // trip, the taker's signer satisfies its descriptor, and the maker's
    // finalize+extract produces a tx Bitcoin Core's mempool accepts.
    let stack = common::stack().await;
    let asset = stack.asset();

    // --- Maker side: mint invoice + reserve RGB inputs --------------------
    let (invoice, maker_outpoints) = {
        let maker = stack.maker_backend().await;
        let invoice = maker
            .create_invoice(&asset, 100)
            .await
            .expect("create_invoice");
        let utxos = maker
            .list_inventory_utxos(&asset)
            .await
            .expect("list_inventory_utxos");
        let outpoints: Vec<Outpoint> = utxos.iter().map(|u| u.outpoint.clone()).collect();
        assert!(
            !outpoints.is_empty(),
            "maker needs at least one RGB allocation to spend"
        );
        (invoice, outpoints)
        // drop MakerGuard → release the lock before taking the TakerGuard
    };

    // --- Taker surfaces a BTC funding input, maker builds the PSBT -------
    let (partial_psbt, consignment, expected_wt) = {
        let taker_btc_input = {
            let taker = stack.taker_backend().await;
            taker
                .spare_btc_input(&asset)
                .await
                .expect("taker should have a non-RGB-bearing funded outpoint")
            // drop TakerGuard → release lock before reacquiring as maker
        };

        let maker = stack.maker_backend().await;
        // Tiny gross + tiny fee: we don't care about value flow here,
        // only that the tx parses + finalizes + broadcasts. Post-issue-#25
        // the seal-anchor BTC value routes back to a maker change output
        // so the actual fee is bounded by `actual_fee_sats` (500), well
        // within Core's default `maxfeerate` cap.
        let transfer = maker
            .create_swap_psbt_buy(
                &invoice,
                100,
                &maker_outpoints,
                std::slice::from_ref(&taker_btc_input),
                stack.taker_funding_addr(),
                1_000,
                500,
                &[],
            )
            .await
            .expect("create_swap_psbt_buy");
        let consignment = transfer.consignment.expect("buy emits consignment");
        let expected_wt = transfer
            .expected_witness_txid
            .clone()
            .expect("declared-funding buy commits a stable witness txid");
        (transfer.partial_psbt, consignment, expected_wt)
    };

    // --- Taker signs + finalizes its own inputs ---------------------------
    let signed_psbt = {
        let taker = stack.taker_backend().await;
        taker
            .sign_and_finalize(&partial_psbt)
            .expect("taker sign+finalize")
    };

    // --- Maker finalizes its own inputs + extracts the witness tx ---------
    let finalized = {
        let maker = stack.maker_backend().await;
        maker
            .finalize_after_taker_sign(&signed_psbt, &consignment)
            .await
            .expect("finalize_after_taker_sign")
    };

    assert_eq!(
        finalized.witness_txid, expected_wt,
        "witness id must be the one committed at PSBT-build (D3 invariant)"
    );
    assert!(!finalized.raw_tx.is_empty(), "raw_tx should be non-empty");
    assert_eq!(
        finalized.final_consignment_base64, consignment,
        "finalize echoes back the original consignment"
    );

    // --- Broadcast: Bitcoin Core accepts the witness tx -------------------
    let broadcast_txid = stack
        .broadcast(&finalized.raw_tx)
        .expect("sendrawtransaction should accept the assembled swap tx");
    assert_eq!(
        broadcast_txid, expected_wt,
        "bitcoind's echoed txid should match the PSBT-committed witness id"
    );
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn chain_observer_enables_consecutive_buys() {
    // Issue #27 / #28: prove the runtime gap is closed at the rfq-rgb
    // layer. Drives buy #1 to completion + broadcast, mines a block,
    // calls `sync_wallet` on both sides (the same call the chain observer
    // loop makes on every tick), then drives buy #2 — which must use the
    // *new* RGB outpoint the maker received as RGB-change from buy #1.
    //
    // Before #27 this test would fail at buy #2: without `sync_wallet`,
    // the bp-wallet UTXO cache stays frozen at startup, so
    // `list_inventory_utxos` after buy #1 returns only the original
    // (already-spent) outpoint and `create_swap_psbt_buy` errors with
    // "reserved RGB outpoint not in maker wallet".
    let stack = common::stack().await;
    let asset = stack.asset();

    let buy = |amount: u64, gross: u64, fee: u64| {
        let asset = asset.clone();
        async move {
            // Reusable single-buy round-trip: returns the witness txid the
            // tx broadcast under.
            let (invoice, maker_outpoints) = {
                let maker = stack.maker_backend().await;
                let invoice = maker
                    .create_invoice(&asset, amount)
                    .await
                    .expect("create_invoice");
                let utxos = maker
                    .list_inventory_utxos(&asset)
                    .await
                    .expect("list_inventory_utxos");
                let outpoints: Vec<Outpoint> = utxos.iter().map(|u| u.outpoint.clone()).collect();
                assert!(
                    !outpoints.is_empty(),
                    "maker should have at least one RGB allocation"
                );
                (invoice, outpoints)
            };

            let (partial_psbt, consignment, expected_wt) = {
                let taker_btc_input = {
                    let taker = stack.taker_backend().await;
                    taker
                        .spare_btc_input(&asset)
                        .await
                        .expect("taker spare BTC input")
                };
                let maker = stack.maker_backend().await;
                let transfer = maker
                    .create_swap_psbt_buy(
                        &invoice,
                        amount,
                        &maker_outpoints,
                        std::slice::from_ref(&taker_btc_input),
                        stack.taker_funding_addr(),
                        gross,
                        fee,
                        &[],
                    )
                    .await
                    .expect("create_swap_psbt_buy");
                let consignment = transfer.consignment.expect("buy emits consignment");
                let expected_wt = transfer
                    .expected_witness_txid
                    .clone()
                    .expect("buy commits a stable witness txid");
                (transfer.partial_psbt, consignment, expected_wt)
            };

            let signed_psbt = {
                let taker = stack.taker_backend().await;
                taker
                    .sign_and_finalize(&partial_psbt)
                    .expect("taker sign+finalize")
            };

            let finalized = {
                let maker = stack.maker_backend().await;
                maker
                    .finalize_after_taker_sign(&signed_psbt, &consignment)
                    .await
                    .expect("finalize_after_taker_sign")
            };
            assert_eq!(finalized.witness_txid, expected_wt);

            let broadcast_txid = stack.broadcast(&finalized.raw_tx).expect("broadcast");
            assert_eq!(broadcast_txid, expected_wt);
            expected_wt
        }
    };

    // --- Buy #1 -----------------------------------------------------------
    let initial_maker_outpoint = {
        let maker = stack.maker_backend().await;
        let utxos = maker
            .list_inventory_utxos(&asset)
            .await
            .expect("list_inventory_utxos");
        utxos
            .first()
            .map(|u| u.outpoint.clone())
            .expect("maker should have an RGB allocation pre-buy")
    };
    let wt1 = buy(100, 1_000, 500).await;

    // --- Mine + sync (what the chain observer loop does on each tick) ---
    stack.mine_block().expect("mine block");
    {
        let maker = stack.maker_backend().await;
        maker.sync_wallet().await.expect("maker sync_wallet");
    }
    {
        let taker = stack.taker_backend().await;
        taker.sync_wallet().await.expect("taker sync_wallet");
    }

    // --- Maker now sees its RGB-change UTXO from buy #1 -------------------
    let new_maker_outpoints: Vec<Outpoint> = {
        let maker = stack.maker_backend().await;
        let utxos = maker
            .list_inventory_utxos(&asset)
            .await
            .expect("list_inventory_utxos post-sync");
        utxos.iter().map(|u| u.outpoint.clone()).collect()
    };
    assert!(
        !new_maker_outpoints.is_empty(),
        "post-sync maker inventory should not be empty — the RGB change from buy #1 should appear"
    );
    assert!(
        !new_maker_outpoints.contains(&initial_maker_outpoint),
        "post-buy maker inventory should NOT still contain the consumed initial outpoint; \
         got new={new_maker_outpoints:?}, initial={initial_maker_outpoint:?}"
    );
    let new_outpoint = &new_maker_outpoints[0];
    assert_eq!(
        new_outpoint.txid, wt1,
        "the new outpoint should be on the witness tx from buy #1"
    );

    // --- Buy #2 — spends the new RGB-change UTXO -------------------------
    // Smaller amount + fee to fit within the post-buy-#1 allocations.
    let wt2 = buy(50, 1_000, 500).await;
    assert_ne!(wt1, wt2, "two distinct broadcasts");
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn sell_round_trip_two_backends_broadcasts() {
    // B5 sell happy path: maker mints invoice → taker uses `rgb transfer`
    // to build a real RGB consignment to the maker → maker validates +
    // composes swap PSBT (taker RGB inputs + maker BTC inputs) → taker
    // signs/finalizes its RGB inputs → maker finalize+extract → bitcoind
    // accepts. Mirrors the buy round-trip, but with the value flow
    // reversed: taker delivers RGB, maker delivers BTC.
    let stack = common::stack().await;
    let asset = stack.asset();

    // --- Maker side: mint invoice + parse it back to typed components -----
    let (maker_invoice, maker_parts, maker_btc_input, maker_payout_addr) = {
        let maker = stack.maker_backend().await;
        let maker_invoice = maker
            .create_invoice(&asset, 200)
            .await
            .expect("create_invoice");
        let parts = maker
            .parse_maker_invoice(&maker_invoice)
            .await
            .expect("parse_maker_invoice");
        let maker_btc_op = maker.spare_btc_outpoint().await;
        let maker_btc_input = (
            maker_btc_op,
            // Real prevout data isn't needed at this layer — swap.rs
            // resolves the value/spk from the wallet via the maker's
            // descriptor in `resolve_maker_btc_inputs` /
            // `enrich_psbt_input`. The `TxOut` here is a placeholder.
            TxOut {
                value_sats: 0,
                script_pubkey: vec![],
            },
        );
        (
            maker_invoice,
            parts,
            maker_btc_input,
            stack.taker_payout_addr().to_owned(),
        )
        // drop MakerGuard → release lock for the taker
    };

    // --- Taker mints a change invoice + builds the consignment ----------
    // The taker's 500 RGB exceeds the maker's 200 invoice, so a change
    // invoice is needed for the 300 surplus. Its `amount` field is
    // irrelevant — swap.rs only reads the beneficiary seal off it.
    let (taker_change_invoice, consignment_b64) = {
        let taker = stack.taker_backend().await;
        let change_invoice = taker
            .create_invoice(&asset, 300)
            .await
            .expect("taker create_invoice (change)");

        use base64::Engine as _;
        let bytes = stack
            .taker_consignment_for(&maker_invoice)
            .expect("taker_consignment_for");
        (
            change_invoice,
            base64::engine::general_purpose::STANDARD.encode(bytes),
        )
    };

    // --- Maker validates: returns the taker's *input* outpoints directly,
    // and absorbs the taker's transition into the maker's stash so
    // `contract_assignments_for(taker_inputs)` in `create_swap_psbt_sell`
    // sees the allocations.
    let consignment_info = {
        let maker = stack.maker_backend().await;
        // TODO(provenance): pass the taker's consigned outpoints once this e2e
        // fixture is updated to the provenance model.
        maker
            .validate_incoming_consignment(&consignment_b64, maker_parts.contract_id, &[])
            .await
            .expect("validate_incoming_consignment")
    };
    assert!(
        !consignment_info.outpoints.is_empty(),
        "validate should surface the taker's RGB input outpoints"
    );

    // --- Resolve taker_rgb_prevouts from the validated input outpoints ----
    let taker_rgb_prevouts = {
        let taker = stack.taker_backend().await;
        let mut prevouts = Vec::with_capacity(consignment_info.outpoints.len());
        for op in &consignment_info.outpoints {
            prevouts.push(taker.lookup_prevout(op).expect("lookup_prevout"));
        }
        prevouts
    };

    let (partial_psbt, expected_wt) = {
        let maker = stack.maker_backend().await;
        let transfer = maker
            .create_swap_psbt_sell(
                &consignment_info,
                &taker_rgb_prevouts,
                std::slice::from_ref(&maker_btc_input),
                maker_parts.contract_id,
                200, // deliver_amount — matches the maker invoice
                &maker_payout_addr,
                Some(&taker_change_invoice), // surplus 300 → taker change
                50_000_000,
                500,
                &[],
            )
            .await
            .expect("create_swap_psbt_sell");
        assert!(
            transfer.consignment.is_some(),
            "sell with surplus: maker emits a change consignment for the taker's surplus RGB"
        );
        let expected_wt = transfer
            .expected_witness_txid
            .clone()
            .expect("declared-funding sell commits a stable witness txid");
        (transfer.partial_psbt, expected_wt)
    };

    // --- Taker signs + finalizes its RGB inputs ---------------------------
    let signed_psbt = {
        let taker = stack.taker_backend().await;
        taker
            .sign_and_finalize(&partial_psbt)
            .expect("taker sign+finalize")
    };

    // --- Maker finalizes its own inputs + extracts the witness tx ---------
    // The `original_consignment_base64` on the sell side is the taker's
    // incoming consignment (per docs/swap-psbt-design.md D4).
    let finalized = {
        let maker = stack.maker_backend().await;
        maker
            .finalize_after_taker_sign(&signed_psbt, &consignment_b64)
            .await
            .expect("finalize_after_taker_sign")
    };

    assert_eq!(
        finalized.witness_txid, expected_wt,
        "witness id must be the one committed at PSBT-build (D3 invariant)"
    );
    assert!(!finalized.raw_tx.is_empty(), "raw_tx should be non-empty");

    // --- Broadcast: Bitcoin Core accepts the witness tx -------------------
    let broadcast_txid = stack
        .broadcast(&finalized.raw_tx)
        .expect("sendrawtransaction should accept the assembled sell swap tx");
    assert_eq!(
        broadcast_txid, expected_wt,
        "bitcoind's echoed txid should match the PSBT-committed witness id"
    );
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn buy_round_trip_witness_vout_invoice_broadcasts() {
    // Witness-vout variant of `buy_round_trip_two_backends_broadcasts`: the
    // taker's receive invoice is address-based (a "future seal"), so the maker
    // adds a fresh output to the taker's address and binds the bought RGB to its
    // post-sort vout — no pre-funded taker anchor involved. Proves the maker's
    // witness-vout buy composition parses, finalizes, and broadcasts.
    let stack = common::stack().await;
    let asset = stack.asset();

    let invoice = stack
        .taker_witness_vout_invoice(100)
        .expect("address-based (witness-vout) invoice");

    let maker_outpoints = {
        let maker = stack.maker_backend().await;
        let utxos = maker
            .list_inventory_utxos(&asset)
            .await
            .expect("list_inventory_utxos");
        let outpoints: Vec<Outpoint> = utxos.iter().map(|u| u.outpoint.clone()).collect();
        assert!(!outpoints.is_empty(), "maker needs RGB to spend");
        outpoints
    };

    let (partial_psbt, consignment, expected_wt) = {
        let taker_btc_input = {
            let taker = stack.taker_backend().await;
            taker
                .spare_btc_input(&asset)
                .await
                .expect("taker spare BTC input")
        };
        let maker = stack.maker_backend().await;
        let transfer = maker
            .create_swap_psbt_buy(
                &invoice,
                100,
                &maker_outpoints,
                std::slice::from_ref(&taker_btc_input),
                stack.taker_funding_addr(),
                1_000,
                500,
                &[],
            )
            .await
            .expect("create_swap_psbt_buy (witness-vout)");
        let consignment = transfer.consignment.expect("buy emits consignment");
        let expected_wt = transfer
            .expected_witness_txid
            .clone()
            .expect("declared-funding buy commits a stable witness txid");
        (transfer.partial_psbt, consignment, expected_wt)
    };

    let signed_psbt = {
        let taker = stack.taker_backend().await;
        taker
            .sign_and_finalize(&partial_psbt)
            .expect("taker sign+finalize")
    };

    let finalized = {
        let maker = stack.maker_backend().await;
        maker
            .finalize_after_taker_sign(&signed_psbt, &consignment)
            .await
            .expect("finalize_after_taker_sign")
    };
    assert_eq!(finalized.witness_txid, expected_wt);
    assert!(!finalized.raw_tx.is_empty());

    let broadcast_txid = stack
        .broadcast(&finalized.raw_tx)
        .expect("bitcoind accepts the witness-vout buy swap tx");
    assert_eq!(broadcast_txid, expected_wt);
}

#[tokio::test]
#[ignore = "needs the regtest stack up + tools installed; see file header"]
async fn buy_swap_piggybacks_change_into_a_ladder_rung() {
    // Settlement-piggyback (buy side): the maker's RGB change rides this swap
    // split into a denomination-ladder rung on a fresh k0 output + a host
    // remainder, instead of one change allocation. Full two-backend flow, then
    // assert the rung lands as recognized maker inventory.
    let stack = common::stack().await;
    let asset = stack.asset();

    // Sell `amount` (= T/4) to the taker; change = 3T/4, split into one rung
    // (= T/2) + a T/4 host remainder (rung distinct from remainder).
    let (invoice, maker_outpoints, amount, rung) = {
        let maker = stack.maker_backend().await;
        let inv = maker
            .list_inventory_utxos(&asset)
            .await
            .expect("list_inventory_utxos");
        let total: u64 = inv.iter().map(|u| u.amount).sum();
        assert!(total >= 4, "maker RGB total {total} too small");
        let amount = total / 4;
        let rung = total / 2;
        let invoice = maker
            .create_invoice(&asset, amount)
            .await
            .expect("create_invoice");
        let outpoints: Vec<Outpoint> = inv.iter().map(|u| u.outpoint.clone()).collect();
        (invoice, outpoints, amount, rung)
    };

    let (partial_psbt, consignment, expected_wt) = {
        let taker_btc_input = {
            let taker = stack.taker_backend().await;
            taker
                .spare_btc_input(&asset)
                .await
                .expect("taker spare BTC input")
        };
        let maker = stack.maker_backend().await;
        let transfer = maker
            .create_swap_psbt_buy(
                &invoice,
                amount,
                &maker_outpoints,
                std::slice::from_ref(&taker_btc_input),
                stack.taker_funding_addr(),
                1_000,
                500,
                &[rung],
            )
            .await
            .expect("compose buy with a change rung");
        let consignment = transfer.consignment.expect("buy emits consignment");
        let wt = transfer
            .expected_witness_txid
            .clone()
            .expect("stable witness txid");
        (transfer.partial_psbt, consignment, wt)
    };

    let signed_psbt = {
        let taker = stack.taker_backend().await;
        taker.sign_and_finalize(&partial_psbt).expect("taker sign")
    };
    let finalized = {
        let maker = stack.maker_backend().await;
        maker
            .finalize_after_taker_sign(&signed_psbt, &consignment)
            .await
            .expect("finalize")
    };
    assert_eq!(finalized.witness_txid, expected_wt);
    let broadcast_txid = stack.broadcast(&finalized.raw_tx).expect("broadcast");
    assert_eq!(broadcast_txid, expected_wt);

    // The change rung is recognized maker inventory on the swap tx.
    let maker = stack.maker_backend().await;
    let inv = maker
        .list_inventory_utxos(&asset)
        .await
        .expect("inventory (post)");
    assert!(
        inv.iter()
            .any(|u| u.outpoint.txid == expected_wt && u.amount == rung),
        "expected the change rung ({rung} units) on the swap tx; got {:#?}",
        inv.iter()
            .filter(|u| u.outpoint.txid == expected_wt)
            .map(|u| u.amount)
            .collect::<Vec<_>>()
    );
}

// NOTE: a sell-side BTC-change piggyback e2e would mirror
// `sell_round_trip_two_backends_broadcasts`, but that fixture is order-dependent
// (the taker's RGB consignment flow only resolves in full-suite order — the test
// fails standalone too). The sell-side no-op invariant is covered by
// `maker-node`'s `broker_round_trip` (buy+sell). The BTC-change split is plain
// k0 outputs (no RGB transition); its arithmetic is value-conserving by the same
// assemble path the buy-side e2e exercises. Left as a follow-up under #35.
