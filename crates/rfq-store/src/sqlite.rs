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

use crate::{
    side_str, BtcInventoryStore, ConsignmentError, ConsignmentRecord, ConsignmentStore, FillError,
    FillRecord, FillStore, InMemoryBtcInventoryStore, InMemoryInventoryStore, InventoryStore,
    OrderError, OrderRecord, OrderStore,
};
use rfq_types::Side;

fn inv(e: impl std::fmt::Display) -> InventoryError {
    InventoryError::Backend(e.to_string())
}

fn cons(e: impl std::fmt::Display) -> ConsignmentError {
    ConsignmentError::Backend(e.to_string())
}

fn ordr(e: impl std::fmt::Display) -> OrderError {
    OrderError::Backend(e.to_string())
}

fn fill(e: impl std::fmt::Display) -> FillError {
    FillError::Backend(e.to_string())
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

// ---------------------------------------------------------------------------
// Consignment store
// ---------------------------------------------------------------------------

/// Durable [`ConsignmentStore`] over SQLite. Unlike the inventory stores there's
/// no hot-read path, so it queries SQL directly (no in-memory working copy) and
/// upserts one row per quote.
pub struct SqliteConsignmentStore {
    pool: SqlitePool,
}

impl SqliteConsignmentStore {
    pub async fn open(path: &Path) -> Result<Self, ConsignmentError> {
        let pool = open_pool(path).await.map_err(cons)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS consignments (\
                 quote_id TEXT PRIMARY KEY, contract_id TEXT NOT NULL, \
                 witness_txid TEXT NOT NULL, consignment TEXT NOT NULL, \
                 created_at_ms INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await
        .map_err(cons)?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_consignments_witness ON consignments (witness_txid)")
            .execute(&pool)
            .await
            .map_err(cons)?;
        Ok(Self { pool })
    }
}

type ConsignmentRow = (String, String, String, String, i64);

fn row_to_record(r: ConsignmentRow) -> ConsignmentRecord {
    ConsignmentRecord {
        quote_id: QuoteId(r.0),
        contract_id: r.1,
        witness_txid: r.2,
        consignment: r.3,
        created_at_ms: r.4 as u64,
    }
}

const SELECT_COLS: &str = "SELECT quote_id, contract_id, witness_txid, consignment, created_at_ms FROM consignments";

#[async_trait]
impl ConsignmentStore for SqliteConsignmentStore {
    async fn save_consignment(&self, record: ConsignmentRecord) -> Result<(), ConsignmentError> {
        sqlx::query(
            "INSERT OR REPLACE INTO consignments \
                 (quote_id, contract_id, witness_txid, consignment, created_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&record.quote_id.0)
        .bind(&record.contract_id)
        .bind(&record.witness_txid)
        .bind(&record.consignment)
        .bind(record.created_at_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(cons)?;
        Ok(())
    }

    async fn get_consignment(
        &self,
        quote_id: &QuoteId,
    ) -> Result<Option<ConsignmentRecord>, ConsignmentError> {
        let row: Option<ConsignmentRow> =
            sqlx::query_as(&format!("{SELECT_COLS} WHERE quote_id = ?1"))
                .bind(&quote_id.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(cons)?;
        Ok(row.map(row_to_record))
    }

    async fn get_by_witness(
        &self,
        witness_txid: &str,
    ) -> Result<Vec<ConsignmentRecord>, ConsignmentError> {
        let rows: Vec<ConsignmentRow> =
            sqlx::query_as(&format!("{SELECT_COLS} WHERE witness_txid = ?1"))
                .bind(witness_txid)
                .fetch_all(&self.pool)
                .await
                .map_err(cons)?;
        Ok(rows.into_iter().map(row_to_record).collect())
    }
}

// ---------------------------------------------------------------------------
// Order store
// ---------------------------------------------------------------------------

/// Durable [`OrderStore`] over SQLite. One row per `(asset_id, side)` (the PK),
/// so `upsert` is INSERT OR REPLACE. WAL (via `open_pool`) lets the `order` CLI
/// write while the daemon reads.
pub struct SqliteOrderStore {
    pool: SqlitePool,
}

impl SqliteOrderStore {
    pub async fn open(path: &Path) -> Result<Self, OrderError> {
        let pool = open_pool(path).await.map_err(ordr)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS orders (\
                 asset_id TEXT NOT NULL, side TEXT NOT NULL, id TEXT NOT NULL, \
                 price INTEGER NOT NULL, size INTEGER NOT NULL, \
                 created_at_ms INTEGER NOT NULL, mirror INTEGER NOT NULL DEFAULT 0, \
                 mirror_spread_bps INTEGER NOT NULL DEFAULT 0, \
                 PRIMARY KEY (asset_id, side))",
        )
        .execute(&pool)
        .await
        .map_err(ordr)?;
        Ok(Self { pool })
    }
}

type OrderRow = (String, String, String, i64, i64, i64, i64, i64);

fn row_to_order(r: OrderRow) -> OrderRecord {
    OrderRecord {
        id: r.0,
        side: r.1,
        asset_id: r.2,
        price: r.3 as u64,
        size: r.4 as u64,
        created_at_ms: r.5 as u64,
        mirror: r.6 != 0,
        mirror_spread_bps: r.7 as u16,
    }
}

const ORDER_COLS: &str =
    "SELECT id, side, asset_id, price, size, created_at_ms, mirror, mirror_spread_bps FROM orders";

#[async_trait]
impl OrderStore for SqliteOrderStore {
    async fn list(&self) -> Result<Vec<OrderRecord>, OrderError> {
        let rows: Vec<OrderRow> = sqlx::query_as(ORDER_COLS)
            .fetch_all(&self.pool)
            .await
            .map_err(ordr)?;
        Ok(rows.into_iter().map(row_to_order).collect())
    }

    async fn upsert(&self, order: OrderRecord) -> Result<(), OrderError> {
        sqlx::query(
            "INSERT OR REPLACE INTO orders \
                 (asset_id, side, id, price, size, created_at_ms, mirror, mirror_spread_bps) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(&order.asset_id)
        .bind(order.side.to_ascii_lowercase())
        .bind(&order.id)
        .bind(order.price as i64)
        .bind(order.size as i64)
        .bind(order.created_at_ms as i64)
        .bind(order.mirror as i64)
        .bind(order.mirror_spread_bps as i64)
        .execute(&self.pool)
        .await
        .map_err(ordr)?;
        Ok(())
    }

    async fn cancel(&self, id: &str) -> Result<bool, OrderError> {
        let res = sqlx::query("DELETE FROM orders WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(ordr)?;
        Ok(res.rows_affected() > 0)
    }

    async fn clear(&self) -> Result<usize, OrderError> {
        let res = sqlx::query("DELETE FROM orders")
            .execute(&self.pool)
            .await
            .map_err(ordr)?;
        Ok(res.rows_affected() as usize)
    }

    async fn get(&self, asset_id: &str, side: &str) -> Result<Option<OrderRecord>, OrderError> {
        let row: Option<OrderRow> =
            sqlx::query_as(&format!("{ORDER_COLS} WHERE asset_id = ?1 AND side = ?2"))
                .bind(asset_id)
                .bind(side.to_ascii_lowercase())
                .fetch_optional(&self.pool)
                .await
                .map_err(ordr)?;
        Ok(row.map(row_to_order))
    }
}

// ---------------------------------------------------------------------------
// Fill store
// ---------------------------------------------------------------------------

/// Durable [`FillStore`] over SQLite. One row per `quote_id` (the PK), so
/// `record_fill` is INSERT OR REPLACE (idempotent on a re-submitted `/sign`).
pub struct SqliteFillStore {
    pool: SqlitePool,
}

impl SqliteFillStore {
    pub async fn open(path: &Path) -> Result<Self, FillError> {
        let pool = open_pool(path).await.map_err(fill)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS fills (\
                 quote_id TEXT PRIMARY KEY, asset_id TEXT NOT NULL, side TEXT NOT NULL, \
                 amount INTEGER NOT NULL, price INTEGER NOT NULL, witness_txid TEXT NOT NULL, \
                 filled_at_ms INTEGER NOT NULL, mirrored INTEGER NOT NULL DEFAULT 0)",
        )
        .execute(&pool)
        .await
        .map_err(fill)?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_fills_asset_side ON fills (asset_id, side)")
            .execute(&pool)
            .await
            .map_err(fill)?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_fills_mirrored ON fills (mirrored)")
            .execute(&pool)
            .await
            .map_err(fill)?;
        Ok(Self { pool })
    }
}

type FillRow = (String, String, String, i64, i64, String, i64, i64);

fn row_to_fill(r: FillRow) -> FillRecord {
    FillRecord {
        quote_id: QuoteId(r.0),
        asset_id: r.1,
        side: crate::parse_side_str(&r.2).unwrap_or(Side::Buy),
        amount: r.3 as u64,
        price: r.4 as u64,
        witness_txid: r.5,
        filled_at_ms: r.6 as u64,
        mirrored: r.7 != 0,
    }
}

const FILL_COLS: &str =
    "SELECT quote_id, asset_id, side, amount, price, witness_txid, filled_at_ms, mirrored FROM fills";

#[async_trait]
impl FillStore for SqliteFillStore {
    async fn record_fill(&self, record: FillRecord) -> Result<(), FillError> {
        sqlx::query(
            "INSERT OR REPLACE INTO fills \
                 (quote_id, asset_id, side, amount, price, witness_txid, filled_at_ms, mirrored) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(&record.quote_id.0)
        .bind(&record.asset_id)
        .bind(side_str(&record.side))
        .bind(record.amount as i64)
        .bind(record.price as i64)
        .bind(&record.witness_txid)
        .bind(record.filled_at_ms as i64)
        .bind(record.mirrored as i64)
        .execute(&self.pool)
        .await
        .map_err(fill)?;
        Ok(())
    }

    async fn filled_for(
        &self,
        asset_id: &str,
        side: &Side,
        since_ms: u64,
    ) -> Result<u64, FillError> {
        let total: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(amount), 0) FROM fills \
                 WHERE asset_id = ?1 AND side = ?2 AND filled_at_ms >= ?3",
        )
        .bind(asset_id)
        .bind(side_str(side))
        .bind(since_ms as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(fill)?;
        Ok(total as u64)
    }

    async fn list_unmirrored(&self) -> Result<Vec<FillRecord>, FillError> {
        let rows: Vec<FillRow> = sqlx::query_as(&format!("{FILL_COLS} WHERE mirrored = 0"))
            .fetch_all(&self.pool)
            .await
            .map_err(fill)?;
        Ok(rows.into_iter().map(row_to_fill).collect())
    }

    async fn mark_mirrored(&self, quote_id: &QuoteId) -> Result<(), FillError> {
        sqlx::query("UPDATE fills SET mirrored = 1 WHERE quote_id = ?1")
            .bind(&quote_id.0)
            .execute(&self.pool)
            .await
            .map_err(fill)?;
        Ok(())
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

    #[tokio::test]
    async fn consignment_survives_reopen_and_is_indexed_by_witness() {
        let path = temp_db();
        let rec = ConsignmentRecord {
            quote_id: QuoteId("q-1".into()),
            contract_id: "rgb:contract-A".into(),
            witness_txid: "wt-1".into(),
            consignment: "Y29uc2lnbm1lbnQ=".into(),
            created_at_ms: NOW_MS,
        };
        {
            let store = SqliteConsignmentStore::open(&path).await.unwrap();
            store.save_consignment(rec.clone()).await.unwrap();
            // Upsert: re-saving the same quote replaces, not duplicates.
            store.save_consignment(rec.clone()).await.unwrap();
        }
        // Reopen → the record is recovered (durable across restart).
        let store = SqliteConsignmentStore::open(&path).await.unwrap();
        assert_eq!(
            store.get_consignment(&QuoteId("q-1".into())).await.unwrap(),
            Some(rec.clone())
        );
        assert!(store
            .get_consignment(&QuoteId("missing".into()))
            .await
            .unwrap()
            .is_none());
        let by_wit = store.get_by_witness("wt-1").await.unwrap();
        assert_eq!(by_wit, vec![rec]);
        let _ = std::fs::remove_file(&path);
    }

    fn order(id: &str, side: &str, price: u64, mirror: bool) -> OrderRecord {
        OrderRecord {
            id: id.into(),
            side: side.into(),
            asset_id: "rgb:contract-A".into(),
            price,
            size: 1000,
            created_at_ms: NOW_MS,
            mirror,
            mirror_spread_bps: if mirror { 200 } else { 0 },
        }
    }

    #[tokio::test]
    async fn order_store_upsert_cancel_survive_reopen() {
        let path = temp_db();
        {
            let store = SqliteOrderStore::open(&path).await.unwrap();
            store.upsert(order("ord-a", "buy", 20, true)).await.unwrap();
            store.upsert(order("ord-b", "sell", 18, false)).await.unwrap();
            // Upsert same (asset, side) replaces (one per side).
            store.upsert(order("ord-c", "buy", 25, true)).await.unwrap();
        }
        let store = SqliteOrderStore::open(&path).await.unwrap();
        let all = store.list().await.unwrap();
        assert_eq!(all.len(), 2, "buy was replaced, sell coexists");
        let buy = store.get("rgb:contract-A", "buy").await.unwrap().unwrap();
        assert_eq!((buy.id.as_str(), buy.price, buy.mirror, buy.mirror_spread_bps), ("ord-c", 25, true, 200));
        assert!(store.cancel("ord-b").await.unwrap());
        assert!(!store.cancel("nope").await.unwrap());
        assert_eq!(store.list().await.unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn fill_store_sums_and_tracks_mirrored() {
        let path = temp_db();
        let mk = |q: &str, side: Side, amount: u64| FillRecord {
            quote_id: QuoteId(q.into()),
            asset_id: "rgb:contract-A".into(),
            side,
            amount,
            price: amount * 20,
            witness_txid: "wt".into(),
            filled_at_ms: NOW_MS,
            mirrored: false,
        };
        {
            let store = SqliteFillStore::open(&path).await.unwrap();
            store.record_fill(mk("q1", Side::Buy, 100)).await.unwrap();
            store.record_fill(mk("q2", Side::Buy, 50)).await.unwrap();
            store.record_fill(mk("q3", Side::Sell, 30)).await.unwrap();
            // Idempotent: re-recording q1 doesn't double-count.
            store.record_fill(mk("q1", Side::Buy, 100)).await.unwrap();
        }
        let store = SqliteFillStore::open(&path).await.unwrap();
        assert_eq!(store.filled_for("rgb:contract-A", &Side::Buy, 0).await.unwrap(), 150);
        assert_eq!(store.filled_for("rgb:contract-A", &Side::Sell, 0).await.unwrap(), 30);
        assert_eq!(store.filled_for("rgb:contract-A", &Side::Buy, NOW_MS + 1).await.unwrap(), 0);
        assert_eq!(store.list_unmirrored().await.unwrap().len(), 3);
        store.mark_mirrored(&QuoteId("q1".into())).await.unwrap();
        let unmirrored = store.list_unmirrored().await.unwrap();
        assert_eq!(unmirrored.len(), 2);
        assert!(unmirrored.iter().all(|f| f.quote_id.0 != "q1"));
        let _ = std::fs::remove_file(&path);
    }
}
