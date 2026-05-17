//! Integration tests for [`LibRgbBackend`] against a live regtest Docker stack.
//!
//! These tests are `#[ignore]` by default so `cargo test --workspace` stays
//! fast and offline. Run them explicitly after bringing the stack up:
//!
//! ```bash
//! make -C infra/regtest regtest-up
//! make -C infra/regtest regtest-mine BLOCKS=103
//! make -C infra/regtest rgb-fund-wallets
//! # (manual rgb_issuer create + import + issue per docs/regtest-rgb20-nia-dev-infra.md)
//!
//! export REGTEST_DIR="$(git rev-parse --show-toplevel)/infra/regtest"
//! export RGB_DATA_DIR="$REGTEST_DIR/data/maker"
//! export RGB_CONTRACT_ID="rgb:<paste-from-rgb_issuer-contracts>"
//! cargo test -p rfq-rgb -- --ignored
//! ```

use std::path::PathBuf;

use rfq_rgb::{LibRgbBackend, RgbBackend, RgbError};
use rfq_types::{AssetId, AssetKind, BitcoinNetwork};

fn env_or_skip(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => {
            eprintln!(
                "skipping: {key} is not set. Export REGTEST_DIR, RGB_DATA_DIR, RGB_CONTRACT_ID \
                 (and optionally ELECTRUM_URL, RGB_WALLET, RGB_NETWORK) per crates/rfq-rgb/tests/cli.rs."
            );
            None
        }
    }
}

fn lib_backend() -> Option<(LibRgbBackend, AssetId)> {
    let data_dir = PathBuf::from(env_or_skip("RGB_DATA_DIR")?);
    let contract_id = env_or_skip("RGB_CONTRACT_ID")?;
    let electrum_url =
        std::env::var("ELECTRUM_URL").unwrap_or_else(|_| "localhost:50001".to_owned());
    let wallet_name = std::env::var("RGB_WALLET").unwrap_or_else(|_| "maker".to_owned());
    let network = std::env::var("RGB_NETWORK").unwrap_or_else(|_| "regtest".to_owned());
    let signer_account_file =
        PathBuf::from(std::env::var("RGB_SIGNER_ACCOUNT_FILE").unwrap_or_default());
    let signer_password = std::env::var("RGB_SIGNER_PASSWORD").unwrap_or_default();

    let backend = LibRgbBackend::new(
        data_dir,
        wallet_name,
        network,
        electrum_url,
        signer_account_file,
        signer_password,
    );
    let asset = AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Rgb20,
        id: contract_id,
    };
    Some((backend, asset))
}

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
#[ignore = "requires the regtest stack with a NIA contract issued; see file header"]
async fn list_inventory_utxos_returns_per_utxo_outpoints() {
    let Some((backend, asset)) = lib_backend() else {
        return;
    };

    let utxos = backend
        .list_inventory_utxos(&asset)
        .await
        .expect("list_inventory_utxos should succeed against a live stash");

    assert!(
        !utxos.is_empty(),
        "expected at least one UTXO; is the contract issued and has the maker received any?"
    );

    let total: u64 = utxos.iter().map(|u| u.amount).sum();
    assert!(total > 0, "expected positive total amount; got {total}");

    let zero_txid = "0".repeat(64);
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
#[ignore = "requires the regtest stack with a NIA contract + a funded maker keychain-9 outpoint"]
async fn create_invoice_returns_rgb_invoice_referencing_the_contract() {
    let Some((backend, asset)) = lib_backend() else {
        return;
    };

    let invoice = backend
        .create_invoice(&asset, 100)
        .await
        .expect("create_invoice should succeed against a live wallet");

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
#[ignore = "requires the regtest stack post-`make rgb-transfer-maker` (consignment + invoice on disk)"]
async fn validate_incoming_consignment_accepts_a_real_consignment() {
    use base64::Engine as _;

    let Some((backend, _asset)) = lib_backend() else {
        return;
    };

    // `rgb-transfer-maker` drops both files into the maker's data dir; the
    // consignment is strict-encoded binary, the invoice is the plain rgb: URI.
    let data_dir = PathBuf::from(std::env::var("RGB_DATA_DIR").expect("RGB_DATA_DIR"));
    let consignment_bytes = std::fs::read(data_dir.join("consignment.rgb"))
        .expect("run `make rgb-transfer-maker` first to produce consignment.rgb");
    let consignment_b64 =
        base64::engine::general_purpose::STANDARD.encode(consignment_bytes);
    let invoice = std::fs::read_to_string(data_dir.join("invoice.txt"))
        .expect("run `make rgb-transfer-maker` first to produce invoice.txt");
    let invoice = invoice.trim();

    let info = backend
        .validate_incoming_consignment(&consignment_b64, invoice)
        .await
        .expect("validate_incoming_consignment should accept a real consignment");

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
#[ignore = "requires the regtest stack with a NIA contract + a funded maker; see file header"]
async fn create_swap_psbt_buy_produces_psbt_and_consignment() {
    let Some((backend, asset)) = lib_backend() else {
        return;
    };

    // Smoke test of the full RGB composition path: use a maker-minted invoice as
    // the (stand-in) beneficiary and the maker's own inventory as the RGB
    // inputs. taker_btc_inputs is empty here — the BTC side is exercised
    // end-to-end at the rfq-maker layer; this asserts the RGB transition build +
    // embed + commit + transfer + maker-sign all succeed and yield a stable
    // witness txid.
    let invoice = backend
        .create_invoice(&asset, 100)
        .await
        .expect("create_invoice should succeed against a live wallet");
    let utxos = backend
        .list_inventory_utxos(&asset)
        .await
        .expect("list_inventory_utxos should succeed");
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
#[ignore = "blocked on LibRgbBackend::finalize_after_taker_sign (issue #13); flip when impl lands"]
async fn finalize_after_taker_sign_returns_witness_txid() {
    let Some((backend, _asset)) = lib_backend() else {
        return;
    };

    // TODO(#13): plug in a real signed PSBT once create_swap_psbt_buy + an
    // out-of-band sign step exist. For now we assert the stub error.
    let result = backend
        .finalize_after_taker_sign("c2lnbmVk", "Y29uc2lnbm1lbnQ=")
        .await;

    assert!(
        matches!(result, Err(RgbError::FinalizeFailed(_))),
        "stub should return FinalizeFailed error until issue #13 lands"
    );
}
