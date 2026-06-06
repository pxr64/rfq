//! Storage e2e: a quote reserves RGB inventory in the SQLite-backed store, then
//! a simulated daemon restart (drop the maker, reopen the same `maker.db`)
//! recovers that reservation. Proves inventory + reservation state survives a
//! maker restart end-to-end through `Maker::request_quote`, not just at the
//! store API level. Mock-backed (no RGB stack / electrum), so CI-safe.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rfq_btc::MockBitcoinClient;
use rfq_maker::{CoinSelector, GreedyExactFitSelector, Maker};
use rfq_rgb::MockRgbBackend;
use rfq_router::MakerConnector;
use rfq_store::{InventoryStore, SqliteInventoryStore};
use rfq_types::{
    AssetId, AssetKind, BitcoinNetwork, InventoryStatus, InventoryUtxo, MakerId, Outpoint,
    QuoteRequest, RfqId, RgbInventoryUtxo, Side,
};

const NOW_MS: u64 = 1_700_000_000_000;

fn temp_db() -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rfq-maker-persist-{}-{n}.db", std::process::id()))
}

fn asset() -> AssetId {
    AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Rgb20,
        id: "rgb-persist-test".to_owned(),
    }
}

fn btc_asset() -> AssetId {
    AssetId {
        network: BitcoinNetwork::Regtest,
        kind: AssetKind::Btc,
        id: "btc".to_owned(),
    }
}

fn outpoint() -> Outpoint {
    Outpoint::new(format!("{:064x}", 1u64), 0)
}

fn rgb_utxo() -> RgbInventoryUtxo {
    RgbInventoryUtxo {
        outpoint: outpoint(),
        asset_id: asset(),
        amount: 1_000_000,
        btc_sats: 0,
    }
}

fn inv_utxo() -> InventoryUtxo {
    InventoryUtxo {
        outpoint: outpoint(),
        asset_id: asset(),
        amount: 1_000_000,
        btc_sats: 0,
        status: InventoryStatus::Available,
        created_at_ms: NOW_MS,
        updated_at_ms: NOW_MS,
        pending_txid: None,
    }
}

fn build_maker(store: Arc<dyn InventoryStore>) -> Maker {
    let rgb_backend = Arc::new(MockRgbBackend::new(vec![rgb_utxo()]));
    let bitcoin_client = Arc::new(MockBitcoinClient::new());
    let selector: Arc<dyn CoinSelector> = Arc::new(GreedyExactFitSelector);
    Maker::with_components(
        MakerId("persist-maker".to_owned()),
        store,
        selector,
        rgb_backend,
        bitcoin_client,
    )
}

#[tokio::test]
async fn reservation_from_quote_survives_maker_restart() {
    let path = temp_db();

    // First boot: seed the durable store, build the maker, take a buy quote
    // (which reserves the RGB UTXO and writes through to maker.db).
    let quote_id = {
        let store = SqliteInventoryStore::open(&path).await.unwrap();
        store
            .replace_for_asset(&asset(), vec![inv_utxo()])
            .await
            .unwrap();
        let maker = build_maker(Arc::new(store));
        let quote = maker
            .request_quote(QuoteRequest {
                rfq_id: RfqId("rfq-1".to_owned()),
                base_asset: asset(),
                quote_asset: btc_asset(),
                side: Side::Buy,
                amount: 100,
                created_at_ms: NOW_MS,
            })
            .await
            .expect("request_quote should not error")
            .expect("maker should quote the buy");
        quote.quote_id
    }; // maker + store dropped → the daemon "stops"

    // Second boot: reopen the SAME db. The pre-restart reservation must be
    // recovered from disk — not reset to Available.
    let store = SqliteInventoryStore::open(&path).await.unwrap();
    assert!(
        store.find_reservation_for_quote(&quote_id).await.is_some(),
        "a reservation taken before restart must survive reopening maker.db"
    );
    assert!(
        store.list_available(&asset()).await.is_empty(),
        "the reserved UTXO must not reappear as Available after restart"
    );

    // A fresh maker built on the reopened store sees the depleted inventory.
    let maker = build_maker(Arc::new(SqliteInventoryStore::open(&path).await.unwrap()));
    let snapshot = maker.inventory_summary().await;
    assert_eq!(snapshot.reserved_amount, 1_000_000);
    assert_eq!(snapshot.available_amount, 0);

    let _ = std::fs::remove_file(&path);
}
