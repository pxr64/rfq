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
use rfq_types::{AssetId, AssetKind, BitcoinNetwork, MakerId};

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
    let maker_id = MakerId(
        std::env::var("MAKER_ID").unwrap_or_else(|_| "test-maker".to_owned()),
    );

    let backend = LibRgbBackend::new(data_dir, wallet_name, network, electrum_url, maker_id);
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
        MakerId("test".to_owned()),
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
async fn list_allocations_returns_seeded_balance() {
    let Some((backend, asset)) = lib_backend() else {
        return;
    };

    let allocations = backend
        .list_allocations(&asset)
        .await
        .expect("list_allocations should succeed against a live stash");

    assert!(
        !allocations.is_empty(),
        "expected at least one allocation; is the contract issued and has the maker received any?"
    );

    let total: u64 = allocations.iter().map(|a| a.available_amount).sum();
    assert!(total > 0, "expected positive total available_amount; got {total}");

    for allocation in &allocations {
        assert_eq!(allocation.asset, asset);
    }
}

#[tokio::test]
#[ignore = "blocked on LibRgbBackend::create_transfer (issue #13); flip when impl lands"]
async fn create_transfer_produces_psbt_and_consignment() {
    let Some((backend, _asset)) = lib_backend() else {
        return;
    };

    // TODO(#13): generate a fresh taker invoice via the rgb-api directly so we
    // have a real beneficiary to transfer to. For now this test just documents
    // the intended shape and is skipped via #[ignore].
    let result = backend
        .create_transfer("rgb:dummy/~/XabF/bcrt:utxob:dummy", 1000)
        .await;

    assert!(
        matches!(result, Err(RgbError::TransferBuild(_))),
        "stub should return TransferBuild error until issue #13 lands"
    );
}

#[tokio::test]
#[ignore = "blocked on LibRgbBackend::finalize_and_broadcast (issue #13); flip when impl lands"]
async fn finalize_and_broadcast_returns_witness_txid() {
    let Some((backend, _asset)) = lib_backend() else {
        return;
    };

    // TODO(#13): plug in a real signed PSBT once create_transfer + an out-of-
    // band sign step exist. For now we assert the stub error.
    let result = backend.finalize_and_broadcast(&[]).await;

    assert!(
        matches!(result, Err(RgbError::BroadcastFailed(_))),
        "stub should return BroadcastFailed error until issue #13 lands"
    );
}
