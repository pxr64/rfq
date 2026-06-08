//! Postgres-backed [`SettlementStore`] for the broker (the maker stays on SQLite —
//! see `project_broker_datastore`). Mirrors the runtime-query approach of
//! `sqlite.rs` (no compile-time `query!` macros, so the build needs no live DB).
//!
//! `AssetId` is stored as JSON for a faithful round-trip, with denormalized
//! `base_asset_id`/`quote_asset_id` columns for cheap filtering; `side`/`status`
//! are stable lowercase strings. Amounts are `BIGINT` (Postgres has no u64).

use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::{QueryBuilder, Row};

use rfq_types::{MakerId, QuoteId, Side, SettlementStatus};

use crate::{Page, SettlementError, SettlementFilter, SettlementRecord, SettlementStore};

fn pg(e: impl std::fmt::Display) -> SettlementError {
    SettlementError::Backend(e.to_string())
}

fn side_str(s: Side) -> &'static str {
    match s {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn parse_side(s: &str) -> Side {
    match s {
        "sell" => Side::Sell,
        _ => Side::Buy,
    }
}

fn status_str(s: SettlementStatus) -> &'static str {
    match s {
        SettlementStatus::Pending => "pending",
        SettlementStatus::Accepted => "accepted",
        SettlementStatus::AwaitingConsignment => "awaiting_consignment",
        SettlementStatus::AwaitingTakerSignature => "awaiting_taker_signature",
        SettlementStatus::PendingBitcoinConfirm => "pending_bitcoin_confirm",
        SettlementStatus::Settled => "settled",
        SettlementStatus::Failed => "failed",
    }
}

fn parse_status(s: &str) -> SettlementStatus {
    match s {
        "accepted" => SettlementStatus::Accepted,
        "awaiting_consignment" => SettlementStatus::AwaitingConsignment,
        "awaiting_taker_signature" => SettlementStatus::AwaitingTakerSignature,
        "pending_bitcoin_confirm" => SettlementStatus::PendingBitcoinConfirm,
        "settled" => SettlementStatus::Settled,
        "failed" => SettlementStatus::Failed,
        _ => SettlementStatus::Pending,
    }
}

const SELECT_COLS: &str = "quote_id, maker_id, base_asset, quote_asset, side, amount, price, \
     fee_sats, witness_txid, status, confirmed_height, mid, quote_count, created_at_ms, updated_at_ms";

fn row_to_record(row: &PgRow) -> Result<SettlementRecord, SettlementError> {
    let base_json: String = row.try_get("base_asset").map_err(pg)?;
    let quote_json: String = row.try_get("quote_asset").map_err(pg)?;
    Ok(SettlementRecord {
        quote_id: QuoteId(row.try_get::<String, _>("quote_id").map_err(pg)?),
        maker_id: MakerId(row.try_get::<String, _>("maker_id").map_err(pg)?),
        base_asset: serde_json::from_str(&base_json).map_err(pg)?,
        quote_asset: serde_json::from_str(&quote_json).map_err(pg)?,
        side: parse_side(&row.try_get::<String, _>("side").map_err(pg)?),
        amount: row.try_get::<i64, _>("amount").map_err(pg)? as u64,
        price: row.try_get::<i64, _>("price").map_err(pg)? as u64,
        fee_sats: row.try_get::<i64, _>("fee_sats").map_err(pg)? as u64,
        witness_txid: row.try_get::<Option<String>, _>("witness_txid").map_err(pg)?,
        status: parse_status(&row.try_get::<String, _>("status").map_err(pg)?),
        confirmed_height: row
            .try_get::<Option<i32>, _>("confirmed_height")
            .map_err(pg)?
            .map(|h| h as u32),
        mid: row.try_get::<Option<i64>, _>("mid").map_err(pg)?.map(|m| m as u64),
        quote_count: row
            .try_get::<Option<i32>, _>("quote_count")
            .map_err(pg)?
            .map(|c| c as u32),
        created_at_ms: row.try_get::<i64, _>("created_at_ms").map_err(pg)? as u64,
        updated_at_ms: row.try_get::<i64, _>("updated_at_ms").map_err(pg)? as u64,
    })
}

/// Durable [`SettlementStore`] over Postgres. Open with the broker's
/// `BROKER_DATABASE_URL`.
pub struct PostgresSettlementStore {
    pool: PgPool,
}

impl PostgresSettlementStore {
    pub async fn open(database_url: &str) -> Result<Self, SettlementError> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await
            .map_err(pg)?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS settlements (\
                 quote_id TEXT PRIMARY KEY, maker_id TEXT NOT NULL, \
                 base_asset TEXT NOT NULL, quote_asset TEXT NOT NULL, \
                 base_asset_id TEXT NOT NULL, quote_asset_id TEXT NOT NULL, \
                 side TEXT NOT NULL, amount BIGINT NOT NULL, price BIGINT NOT NULL, \
                 fee_sats BIGINT NOT NULL, witness_txid TEXT, status TEXT NOT NULL, \
                 confirmed_height INT, mid BIGINT, quote_count INT, \
                 created_at_ms BIGINT NOT NULL, updated_at_ms BIGINT NOT NULL)",
        )
        .execute(&pool)
        .await
        .map_err(pg)?;
        for idx in [
            "CREATE INDEX IF NOT EXISTS idx_settlements_status ON settlements (status)",
            "CREATE INDEX IF NOT EXISTS idx_settlements_maker ON settlements (maker_id)",
            "CREATE INDEX IF NOT EXISTS idx_settlements_witness ON settlements (witness_txid)",
            "CREATE INDEX IF NOT EXISTS idx_settlements_created ON settlements (created_at_ms)",
            "CREATE INDEX IF NOT EXISTS idx_settlements_base ON settlements (base_asset_id)",
            "CREATE INDEX IF NOT EXISTS idx_settlements_quote ON settlements (quote_asset_id)",
        ] {
            sqlx::query(idx).execute(&pool).await.map_err(pg)?;
        }
        Ok(Self { pool })
    }
}

#[async_trait::async_trait]
impl SettlementStore for PostgresSettlementStore {
    async fn save_settlement(&self, record: SettlementRecord) -> Result<(), SettlementError> {
        // Upsert; ON CONFLICT keeps the original created_at_ms (not in the SET).
        sqlx::query(
            "INSERT INTO settlements (quote_id, maker_id, base_asset, quote_asset, \
                 base_asset_id, quote_asset_id, side, amount, price, fee_sats, witness_txid, \
                 status, confirmed_height, mid, quote_count, created_at_ms, updated_at_ms) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17) \
             ON CONFLICT (quote_id) DO UPDATE SET \
                 maker_id=EXCLUDED.maker_id, base_asset=EXCLUDED.base_asset, \
                 quote_asset=EXCLUDED.quote_asset, base_asset_id=EXCLUDED.base_asset_id, \
                 quote_asset_id=EXCLUDED.quote_asset_id, side=EXCLUDED.side, amount=EXCLUDED.amount, \
                 price=EXCLUDED.price, fee_sats=EXCLUDED.fee_sats, witness_txid=EXCLUDED.witness_txid, \
                 status=EXCLUDED.status, confirmed_height=EXCLUDED.confirmed_height, \
                 mid=COALESCE(EXCLUDED.mid, settlements.mid), \
                 quote_count=COALESCE(EXCLUDED.quote_count, settlements.quote_count), \
                 updated_at_ms=EXCLUDED.updated_at_ms",
        )
        .bind(&record.quote_id.0)
        .bind(&record.maker_id.0)
        .bind(serde_json::to_string(&record.base_asset).map_err(pg)?)
        .bind(serde_json::to_string(&record.quote_asset).map_err(pg)?)
        .bind(&record.base_asset.id)
        .bind(&record.quote_asset.id)
        .bind(side_str(record.side))
        .bind(record.amount as i64)
        .bind(record.price as i64)
        .bind(record.fee_sats as i64)
        .bind(record.witness_txid.as_deref())
        .bind(status_str(record.status))
        .bind(record.confirmed_height.map(|h| h as i32))
        .bind(record.mid.map(|m| m as i64))
        .bind(record.quote_count.map(|c| c as i32))
        .bind(record.created_at_ms as i64)
        .bind(record.updated_at_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(pg)?;
        Ok(())
    }

    async fn update_status(
        &self,
        quote_id: &QuoteId,
        status: SettlementStatus,
        confirmed_height: Option<u32>,
        now_ms: u64,
    ) -> Result<bool, SettlementError> {
        let res = sqlx::query(
            "UPDATE settlements SET status=$1, confirmed_height=$2, updated_at_ms=$3 WHERE quote_id=$4",
        )
        .bind(status_str(status))
        .bind(confirmed_height.map(|h| h as i32))
        .bind(now_ms as i64)
        .bind(&quote_id.0)
        .execute(&self.pool)
        .await
        .map_err(pg)?;
        Ok(res.rows_affected() > 0)
    }

    async fn get_settlement(
        &self,
        quote_id: &QuoteId,
    ) -> Result<Option<SettlementRecord>, SettlementError> {
        let row = sqlx::query(&format!("SELECT {SELECT_COLS} FROM settlements WHERE quote_id = $1"))
            .bind(&quote_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(pg)?;
        row.as_ref().map(row_to_record).transpose()
    }

    async fn list_settlements(
        &self,
        filter: &SettlementFilter,
        page: Page,
    ) -> Result<Vec<SettlementRecord>, SettlementError> {
        let mut qb = QueryBuilder::new(format!("SELECT {SELECT_COLS} FROM settlements WHERE TRUE"));
        if let Some(m) = &filter.maker_id {
            qb.push(" AND maker_id = ").push_bind(m.0.clone());
        }
        if let Some(a) = &filter.base_asset_id {
            qb.push(" AND base_asset_id = ").push_bind(a.clone());
        }
        if let Some(a) = &filter.quote_asset_id {
            qb.push(" AND quote_asset_id = ").push_bind(a.clone());
        }
        if let Some(s) = &filter.side {
            qb.push(" AND side = ").push_bind(side_str(s.clone()));
        }
        if let Some(s) = &filter.status {
            qb.push(" AND status = ").push_bind(status_str(*s));
        }
        if let Some(t) = filter.since_ms {
            qb.push(" AND created_at_ms >= ").push_bind(t as i64);
        }
        qb.push(" ORDER BY created_at_ms DESC, quote_id DESC");
        qb.push(" LIMIT ").push_bind(page.limit as i64);
        qb.push(" OFFSET ").push_bind(page.offset as i64);
        let rows = qb.build().fetch_all(&self.pool).await.map_err(pg)?;
        rows.iter().map(row_to_record).collect()
    }

    async fn pending_witness_txids(&self) -> Result<Vec<(QuoteId, String)>, SettlementError> {
        let rows = sqlx::query(
            "SELECT quote_id, witness_txid FROM settlements \
             WHERE status = $1 AND witness_txid IS NOT NULL",
        )
        .bind(status_str(SettlementStatus::PendingBitcoinConfirm))
        .fetch_all(&self.pool)
        .await
        .map_err(pg)?;
        rows.iter()
            .map(|r| {
                Ok((
                    QuoteId(r.try_get::<String, _>("quote_id").map_err(pg)?),
                    r.try_get::<String, _>("witness_txid").map_err(pg)?,
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Page, SettlementFilter, SettlementStore};
    use rfq_types::{AssetId, AssetKind, BitcoinNetwork};

    fn rec(q: &str, created: u64) -> SettlementRecord {
        SettlementRecord {
            quote_id: QuoteId(q.into()),
            maker_id: MakerId("m1".into()),
            base_asset: AssetId { network: BitcoinNetwork::Regtest, kind: AssetKind::Btc, id: "btc".into() },
            quote_asset: AssetId {
                network: BitcoinNetwork::Regtest,
                kind: AssetKind::Rgb20,
                id: "rgb:AAA".into(),
            },
            side: Side::Buy,
            amount: 1000,
            price: 101,
            fee_sats: 200,
            witness_txid: None,
            status: SettlementStatus::Accepted,
            confirmed_height: None,
            mid: None,
            quote_count: None,
            created_at_ms: created,
            updated_at_ms: created,
        }
    }

    /// Live Postgres round-trip. Skipped unless `TEST_DATABASE_URL` is set, so CI
    /// (no PG) stays green; run against a throwaway container to exercise the real
    /// schema (CREATE TABLE, ON CONFLICT upsert, QueryBuilder filters, type binds).
    #[tokio::test]
    async fn pg_round_trip_upsert_update_list() {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("SKIP pg_round_trip_upsert_update_list: set TEST_DATABASE_URL to run");
            return;
        };
        let store = PostgresSettlementStore::open(&url).await.unwrap();
        sqlx::query("DELETE FROM settlements").execute(&store.pool).await.unwrap();

        // First record carries RFQ stats (as /accept would).
        let mut q1 = rec("q1", 100);
        q1.mid = Some(99);
        q1.quote_count = Some(4);
        store.save_settlement(q1).await.unwrap();
        store.save_settlement(rec("q2", 300)).await.unwrap();

        // Upsert q1 → broadcast + witness; created_at_ms + RFQ stats must survive
        // (the /sign upsert carries None for mid/quote_count).
        let mut adv = rec("q1", 999);
        adv.status = SettlementStatus::PendingBitcoinConfirm;
        adv.witness_txid = Some("wt1".into());
        store.save_settlement(adv).await.unwrap();

        let got = store.get_settlement(&QuoteId("q1".into())).await.unwrap().unwrap();
        assert_eq!(got.created_at_ms, 100, "created_at preserved across upsert");
        assert_eq!(got.mid, Some(99), "mid preserved (COALESCE) across upsert");
        assert_eq!(got.quote_count, Some(4), "quote_count preserved across upsert");
        assert_eq!(got.witness_txid.as_deref(), Some("wt1"));
        assert_eq!(got.base_asset.kind, AssetKind::Btc, "AssetId json round-trips");
        assert_eq!(got.quote_asset.id, "rgb:AAA");

        // Newest first.
        let all = store
            .list_settlements(&SettlementFilter::default(), Page::default())
            .await
            .unwrap();
        assert_eq!(all.iter().map(|r| r.quote_id.0.clone()).collect::<Vec<_>>(), ["q2", "q1"]);

        // Confirmation loop work-list, then promote, then it clears.
        assert_eq!(
            store.pending_witness_txids().await.unwrap(),
            vec![(QuoteId("q1".into()), "wt1".to_owned())]
        );
        assert!(store
            .update_status(&QuoteId("q1".into()), SettlementStatus::Settled, Some(7), 1234)
            .await
            .unwrap());
        let q1 = store.get_settlement(&QuoteId("q1".into())).await.unwrap().unwrap();
        assert_eq!(q1.confirmed_height, Some(7));
        assert_eq!(q1.status, SettlementStatus::Settled);
        assert!(store.pending_witness_txids().await.unwrap().is_empty());

        // Status filter.
        let settled = store
            .list_settlements(
                &SettlementFilter { status: Some(SettlementStatus::Settled), ..Default::default() },
                Page::default(),
            )
            .await
            .unwrap();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].quote_id.0, "q1");
    }
}
