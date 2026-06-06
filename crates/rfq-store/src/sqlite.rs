//! SQLite-backed persistence for the maker's inventory stores.
//!
//! Strategy: keep an `InMemory*` store as the working copy — so reads and the
//! all-or-nothing reservation logic reuse the battle-tested in-memory impls
//! verbatim — and write the whole inventory *through* to SQLite (one JSON-blob
//! row per UTXO) after every mutation, inside a transaction. On `open` we load
//! the persisted rows back into the working copy, so a daemon restart recovers
//! every reservation and settlement status. Mutations serialize on a write
//! lock, so each persisted snapshot is internally consistent and there is no
//! reservation race.
//!
//! Queries/filtering run in Rust against the working copy — fine at maker
//! inventory scale (hundreds of UTXOs). If inventory ever grows large, hot
//! reads can be pushed into indexed SQL behind the same trait without touching
//! callers.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use rfq_types::{
    AssetId, BtcInventoryError, BtcInventorySnapshot, BtcInventoryUtxo, ExtendedInventorySnapshot,
    InventoryError, InventoryUtxo, Outpoint, QuoteId, ReservationId, RfqId,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::{BtcInventoryStore, InMemoryBtcInventoryStore, InMemoryInventoryStore, InventoryStore};

fn inv(e: impl std::fmt::Display) -> InventoryError {
    InventoryError::Backend(e.to_string())
}

fn btc(e: impl std::fmt::Display) -> BtcInventoryError {
    BtcInventoryError::Backend(e.to_string())
}

async fn open_pool(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(10));
    SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
}

// ---------------------------------------------------------------------------
// RGB inventory
// ---------------------------------------------------------------------------

/// Durable [`InventoryStore`] over SQLite. See module docs for the strategy.
pub struct SqliteInventoryStore {
    mem: InMemoryInventoryStore,
    pool: SqlitePool,
    write_lock: Mutex<()>,
}

impl SqliteInventoryStore {
    /// Open (creating if absent) the inventory db at `path` and load any
    /// persisted UTXOs into the in-memory working copy.
    pub async fn open(path: &Path) -> Result<Self, InventoryError> {
        let pool = open_pool(path).await.map_err(inv)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS rgb_utxos (\
                 txid TEXT NOT NULL, vout INTEGER NOT NULL, data TEXT NOT NULL, \
                 PRIMARY KEY (txid, vout))",
        )
        .execute(&pool)
        .await
        .map_err(inv)?;

        let datas: Vec<String> = sqlx::query_scalar("SELECT data FROM rgb_utxos")
            .fetch_all(&pool)
            .await
            .map_err(inv)?;
        let mut utxos = Vec::with_capacity(datas.len());
        for d in &datas {
            utxos.push(serde_json::from_str::<InventoryUtxo>(d).map_err(inv)?);
        }
        Ok(Self {
            mem: InMemoryInventoryStore::with_seed(utxos),
            pool,
            write_lock: Mutex::new(()),
        })
    }

    /// Whether the db holds no UTXOs — lets `build_runtime` distinguish a first
    /// run (seed from chain) from a restart (keep persisted state).
    pub async fn is_empty(&self) -> bool {
        self.mem.list_all().await.is_empty()
    }

    /// Write the whole working copy through to SQLite in one transaction.
    async fn persist(&self) -> Result<(), InventoryError> {
        let utxos = self.mem.list_all().await;
        let mut tx = self.pool.begin().await.map_err(inv)?;
        sqlx::query("DELETE FROM rgb_utxos")
            .execute(&mut *tx)
            .await
            .map_err(inv)?;
        for u in &utxos {
            let data = serde_json::to_string(u).map_err(inv)?;
            sqlx::query("INSERT INTO rgb_utxos (txid, vout, data) VALUES (?1, ?2, ?3)")
                .bind(&u.outpoint.txid)
                .bind(u.outpoint.vout as i64)
                .bind(data)
                .execute(&mut *tx)
                .await
                .map_err(inv)?;
        }
        tx.commit().await.map_err(inv)?;
        Ok(())
    }
}

#[async_trait]
impl InventoryStore for SqliteInventoryStore {
    // --- reads: straight to the working copy ---
    async fn list_for_asset(&self, asset: &AssetId) -> Vec<InventoryUtxo> {
        self.mem.list_for_asset(asset).await
    }
    async fn list_all(&self) -> Vec<InventoryUtxo> {
        self.mem.list_all().await
    }
    async fn list_available(&self, asset: &AssetId) -> Vec<InventoryUtxo> {
        self.mem.list_available(asset).await
    }
    async fn get(&self, outpoint: &Outpoint) -> Option<InventoryUtxo> {
        self.mem.get(outpoint).await
    }
    async fn extended_snapshot(&self, asset: &AssetId) -> ExtendedInventorySnapshot {
        self.mem.extended_snapshot(asset).await
    }
    async fn find_reservation_for_quote(&self, quote_id: &QuoteId) -> Option<ReservationId> {
        self.mem.find_reservation_for_quote(quote_id).await
    }

    // --- mutations: working copy, then write-through (serialized) ---
    async fn replace_for_asset(
        &self,
        asset: &AssetId,
        utxos: Vec<InventoryUtxo>,
    ) -> Result<(), InventoryError> {
        let _g = self.write_lock.lock().await;
        self.mem.replace_for_asset(asset, utxos).await?;
        self.persist().await
    }

    async fn ingest_change_utxo(&self, utxo: InventoryUtxo) -> Result<(), InventoryError> {
        let _g = self.write_lock.lock().await;
        self.mem.ingest_change_utxo(utxo).await?;
        self.persist().await
    }

    async fn reserve_utxos(
        &self,
        rfq_id: &RfqId,
        quote_id: &QuoteId,
        outpoints: &[Outpoint],
        expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<ReservationId, InventoryError> {
        let _g = self.write_lock.lock().await;
        let id = self
            .mem
            .reserve_utxos(rfq_id, quote_id, outpoints, expires_at_ms, now_ms)
            .await?;
        self.persist().await?;
        Ok(id)
    }

    async fn release_reservation(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self.mem.release_reservation(reservation_id, now_ms).await?;
        self.persist().await?;
        Ok(n)
    }

    async fn release_expired_reservations(&self, now_ms: u64) -> usize {
        let _g = self.write_lock.lock().await;
        let n = self.mem.release_expired_reservations(now_ms).await;
        if n > 0 {
            if let Err(e) = self.persist().await {
                eprintln!("sqlite inventory persist (release_expired): {e}");
            }
        }
        n
    }

    async fn extend_reservation(
        &self,
        reservation_id: &ReservationId,
        new_expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self
            .mem
            .extend_reservation(reservation_id, new_expires_at_ms, now_ms)
            .await?;
        self.persist().await?;
        Ok(n)
    }

    async fn mark_spent(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self.mem.mark_spent(reservation_id, witness_txid, now_ms).await?;
        self.persist().await?;
        Ok(n)
    }

    async fn mark_pending_bitcoin_confirm(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self
            .mem
            .mark_pending_bitcoin_confirm(reservation_id, witness_txid, now_ms)
            .await?;
        self.persist().await?;
        Ok(n)
    }

    async fn mark_pending_rgb_acceptance(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self
            .mem
            .mark_pending_rgb_acceptance(reservation_id, now_ms)
            .await?;
        self.persist().await?;
        Ok(n)
    }

    async fn mark_broadcast_failed(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self.mem.mark_broadcast_failed(reservation_id, now_ms).await?;
        self.persist().await?;
        Ok(n)
    }

    async fn mark_rgb_acceptance_failed(
        &self,
        reservation_id: &ReservationId,
        reason: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self
            .mem
            .mark_rgb_acceptance_failed(reservation_id, reason, now_ms)
            .await?;
        self.persist().await?;
        Ok(n)
    }

    async fn mark_reorged(&self, witness_txid: &str, now_ms: u64) -> Result<usize, InventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self.mem.mark_reorged(witness_txid, now_ms).await?;
        self.persist().await?;
        Ok(n)
    }

    async fn mark_invalid(
        &self,
        outpoint: &Outpoint,
        reason: String,
        now_ms: u64,
    ) -> Result<(), InventoryError> {
        let _g = self.write_lock.lock().await;
        self.mem.mark_invalid(outpoint, reason, now_ms).await?;
        self.persist().await
    }
}

// ---------------------------------------------------------------------------
// BTC inventory
// ---------------------------------------------------------------------------

/// Durable [`BtcInventoryStore`] over SQLite. Mirrors [`SqliteInventoryStore`].
pub struct SqliteBtcInventoryStore {
    mem: InMemoryBtcInventoryStore,
    pool: SqlitePool,
    write_lock: Mutex<()>,
}

impl SqliteBtcInventoryStore {
    pub async fn open(path: &Path) -> Result<Self, BtcInventoryError> {
        let pool = open_pool(path).await.map_err(btc)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS btc_utxos (\
                 txid TEXT NOT NULL, vout INTEGER NOT NULL, data TEXT NOT NULL, \
                 PRIMARY KEY (txid, vout))",
        )
        .execute(&pool)
        .await
        .map_err(btc)?;

        let datas: Vec<String> = sqlx::query_scalar("SELECT data FROM btc_utxos")
            .fetch_all(&pool)
            .await
            .map_err(btc)?;
        let mut utxos = Vec::with_capacity(datas.len());
        for d in &datas {
            utxos.push(serde_json::from_str::<BtcInventoryUtxo>(d).map_err(btc)?);
        }
        Ok(Self {
            mem: InMemoryBtcInventoryStore::with_seed(utxos),
            pool,
            write_lock: Mutex::new(()),
        })
    }

    pub async fn is_empty(&self) -> bool {
        self.mem.list_all().await.is_empty()
    }

    async fn persist(&self) -> Result<(), BtcInventoryError> {
        let utxos = self.mem.list_all().await;
        let mut tx = self.pool.begin().await.map_err(btc)?;
        sqlx::query("DELETE FROM btc_utxos")
            .execute(&mut *tx)
            .await
            .map_err(btc)?;
        for u in &utxos {
            let data = serde_json::to_string(u).map_err(btc)?;
            sqlx::query("INSERT INTO btc_utxos (txid, vout, data) VALUES (?1, ?2, ?3)")
                .bind(&u.outpoint.txid)
                .bind(u.outpoint.vout as i64)
                .bind(data)
                .execute(&mut *tx)
                .await
                .map_err(btc)?;
        }
        tx.commit().await.map_err(btc)?;
        Ok(())
    }
}

#[async_trait]
impl BtcInventoryStore for SqliteBtcInventoryStore {
    async fn list_all(&self) -> Vec<BtcInventoryUtxo> {
        self.mem.list_all().await
    }
    async fn list_available(&self) -> Vec<BtcInventoryUtxo> {
        self.mem.list_available().await
    }
    async fn get(&self, outpoint: &Outpoint) -> Option<BtcInventoryUtxo> {
        self.mem.get(outpoint).await
    }
    async fn snapshot(&self) -> BtcInventorySnapshot {
        self.mem.snapshot().await
    }
    async fn find_reservation_for_quote(&self, quote_id: &QuoteId) -> Option<ReservationId> {
        self.mem.find_reservation_for_quote(quote_id).await
    }

    async fn replace_all(&self, utxos: Vec<BtcInventoryUtxo>) {
        let _g = self.write_lock.lock().await;
        self.mem.replace_all(utxos).await;
        if let Err(e) = self.persist().await {
            eprintln!("sqlite btc inventory persist (replace_all): {e}");
        }
    }

    async fn ingest_change_utxo(
        &self,
        utxo: BtcInventoryUtxo,
    ) -> Result<(), BtcInventoryError> {
        let _g = self.write_lock.lock().await;
        self.mem.ingest_change_utxo(utxo).await?;
        self.persist().await
    }

    async fn reserve(
        &self,
        rfq_id: &RfqId,
        quote_id: &QuoteId,
        outpoints: &[Outpoint],
        expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<ReservationId, BtcInventoryError> {
        let _g = self.write_lock.lock().await;
        let id = self
            .mem
            .reserve(rfq_id, quote_id, outpoints, expires_at_ms, now_ms)
            .await?;
        self.persist().await?;
        Ok(id)
    }

    async fn extend_reservation(
        &self,
        reservation_id: &ReservationId,
        new_expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self
            .mem
            .extend_reservation(reservation_id, new_expires_at_ms, now_ms)
            .await?;
        self.persist().await?;
        Ok(n)
    }

    async fn release_reservation(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self.mem.release_reservation(reservation_id, now_ms).await?;
        self.persist().await?;
        Ok(n)
    }

    async fn release_expired_reservations(&self, now_ms: u64) -> usize {
        let _g = self.write_lock.lock().await;
        let n = self.mem.release_expired_reservations(now_ms).await;
        if n > 0 {
            if let Err(e) = self.persist().await {
                eprintln!("sqlite btc inventory persist (release_expired): {e}");
            }
        }
        n
    }

    async fn mark_pending_bitcoin_confirm(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self
            .mem
            .mark_pending_bitcoin_confirm(reservation_id, witness_txid, now_ms)
            .await?;
        self.persist().await?;
        Ok(n)
    }

    async fn mark_spent(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self.mem.mark_spent(reservation_id, witness_txid, now_ms).await?;
        self.persist().await?;
        Ok(n)
    }

    async fn mark_broadcast_failed(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self.mem.mark_broadcast_failed(reservation_id, now_ms).await?;
        self.persist().await?;
        Ok(n)
    }

    async fn mark_reorged(
        &self,
        witness_txid: &str,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError> {
        let _g = self.write_lock.lock().await;
        let n = self.mem.mark_reorged(witness_txid, now_ms).await?;
        self.persist().await?;
        Ok(n)
    }

    async fn mark_invalid(
        &self,
        outpoint: &Outpoint,
        reason: String,
        now_ms: u64,
    ) -> Result<(), BtcInventoryError> {
        let _g = self.write_lock.lock().await;
        self.mem.mark_invalid(outpoint, reason, now_ms).await?;
        self.persist().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_types::{AssetKind, BitcoinNetwork, InventoryStatus};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    const NOW_MS: u64 = 1_700_000_000_000;
    const TXID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn temp_db() -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("rfq-store-sqlite-{}-{n}.db", std::process::id()))
    }

    fn asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-test".to_owned(),
        }
    }

    fn outpoint(vout: u32) -> Outpoint {
        Outpoint::new(TXID, vout)
    }

    fn utxo(vout: u32, amount: u64) -> InventoryUtxo {
        InventoryUtxo {
            outpoint: outpoint(vout),
            asset_id: asset(),
            amount,
            btc_sats: 1000,
            status: InventoryStatus::Available,
            created_at_ms: NOW_MS,
            updated_at_ms: NOW_MS,
            pending_txid: None,
        }
    }

    #[tokio::test]
    async fn reservation_survives_reopen() {
        let path = temp_db();
        {
            let store = SqliteInventoryStore::open(&path).await.unwrap();
            store
                .replace_for_asset(&asset(), vec![utxo(0, 100), utxo(1, 200)])
                .await
                .unwrap();
            store
                .reserve_utxos(
                    &RfqId("rfq-1".into()),
                    &QuoteId("q-1".into()),
                    &[outpoint(0)],
                    NOW_MS + 30_000,
                    NOW_MS,
                )
                .await
                .unwrap();
        }
        // Reopen: the reservation must be recovered from disk.
        let store = SqliteInventoryStore::open(&path).await.unwrap();
        assert!(!store.is_empty().await);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Reserved { .. }
        ));
        assert!(matches!(
            store.get(&outpoint(1)).await.unwrap().status,
            InventoryStatus::Available
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn spent_status_round_trips_through_json() {
        let path = temp_db();
        let store = SqliteInventoryStore::open(&path).await.unwrap();
        store.replace_for_asset(&asset(), vec![utxo(0, 100)]).await.unwrap();
        let rid = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();
        store
            .mark_pending_bitcoin_confirm(&rid, "wt-1".into(), NOW_MS + 100)
            .await
            .unwrap();
        store.mark_spent(&rid, "wt-1".into(), NOW_MS + 200).await.unwrap();

        let reopened = SqliteInventoryStore::open(&path).await.unwrap();
        assert!(matches!(
            reopened.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Spent { ref witness_txid, .. } if witness_txid == "wt-1"
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reserve_is_atomic_under_contention() {
        let path = temp_db();
        let store = Arc::new(SqliteInventoryStore::open(&path).await.unwrap());
        store.replace_for_asset(&asset(), vec![utxo(0, 100)]).await.unwrap();

        // 10 tasks race to reserve the single available UTXO; exactly one wins.
        let mut handles = Vec::new();
        for i in 0..10u32 {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                store
                    .reserve_utxos(
                        &RfqId(format!("rfq-{i}")),
                        &QuoteId(format!("q-{i}")),
                        &[outpoint(0)],
                        NOW_MS + 30_000,
                        NOW_MS,
                    )
                    .await
                    .is_ok()
            }));
        }
        let mut wins = 0;
        for h in handles {
            if h.await.unwrap() {
                wins += 1;
            }
        }
        assert_eq!(wins, 1, "exactly one task may reserve the UTXO");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn btc_reservation_survives_reopen() {
        use rfq_types::BtcInventoryStatus;
        let path = temp_db();
        let u = BtcInventoryUtxo {
            outpoint: outpoint(0),
            value_sats: 5000,
            script_pubkey: vec![0x51],
            status: BtcInventoryStatus::Available,
            created_at_ms: NOW_MS,
            updated_at_ms: NOW_MS,
            pending_txid: None,
        };
        let rid = {
            let store = SqliteBtcInventoryStore::open(&path).await.unwrap();
            store.replace_all(vec![u]).await;
            store
                .reserve(
                    &RfqId("rfq-1".into()),
                    &QuoteId("q-1".into()),
                    &[outpoint(0)],
                    NOW_MS + 30_000,
                    NOW_MS,
                )
                .await
                .unwrap()
        };
        let store = SqliteBtcInventoryStore::open(&path).await.unwrap();
        assert_eq!(
            store.find_reservation_for_quote(&QuoteId("q-1".into())).await,
            Some(rid)
        );
        let _ = std::fs::remove_file(&path);
    }
}
