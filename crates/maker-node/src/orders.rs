//! Standing maker orders: the prices the maker quotes per (asset, side),
//! replacing the flat default markup. Managed via
//! `colorex maker order {create,list,cancel}` and loaded by `maker up` into the
//! maker's [`PricePolicy`](rfq_maker::PricePolicy).
//!
//! Persistence is the `orders` table in maker.db (via [`rfq_store::OrderStore`])
//! — co-located with inventory/consignments/fills, and safe for the `order` CLI
//! to write while the daemon reads (SQLite/WAL). At most one order is kept per
//! (asset, side) — creating a second upserts the first.
//!
//! A legacy `orders.json` (the pre-maker.db format) is auto-imported once via
//! [`migrate_orders_json`].

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rfq_maker::{PriceEntry, PricePolicy};
use rfq_store::{OrderRecord, OrderStore};
use rfq_types::Side;

/// The order record type (defined in rfq-store, where the store lives).
pub use rfq_store::OrderRecord as Order;

/// Parse a side label (case-insensitive) into a [`Side`].
pub fn parse_side(s: &str) -> Option<Side> {
    rfq_store::parse_side_str(s)
}

/// Milliseconds since the Unix epoch — used for order ids + timestamps.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Construct an order with a generated id (hex of the creation timestamp).
pub fn new_order(
    side: &str,
    asset_id: String,
    price: u64,
    size: u64,
    mirror: bool,
    mirror_spread_bps: u16,
) -> OrderRecord {
    let created_at_ms = now_ms();
    OrderRecord {
        id: format!("ord-{created_at_ms:x}"),
        side: side.to_ascii_lowercase(),
        asset_id,
        price,
        size,
        created_at_ms,
        mirror,
        mirror_spread_bps,
    }
}

/// Build the maker's [`PricePolicy`] from the orders with a recognized side.
pub fn price_policy(orders: &[OrderRecord]) -> PricePolicy {
    let entries = orders
        .iter()
        .filter_map(|o| {
            parse_side(&o.side).map(|side| PriceEntry {
                asset_id: o.asset_id.clone(),
                side,
                price_sats_per_unit: o.price,
                max_size: o.size,
            })
        })
        .collect();
    PricePolicy::from_entries(entries)
}

/// The opposite side (buy ⇄ sell).
pub fn opposite(side: Side) -> Side {
    match side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

/// Build the auto-mirror order for a fill: the OPPOSITE side, sized to the
/// filled amount, priced off the fill's per-unit price by `spread_bps`, and
/// itself mirror-enabled so the loop continues.
///
/// - maker SOLD RGB (a `buy` order filled) → mirror is a `sell` buy-back,
///   **cheaper** (`-spread`).
/// - maker BOUGHT RGB (a `sell` order filled) → mirror is a `buy` re-sell,
///   **dearer** (`+spread`).
///
/// `fill_unit_price` is the per-unit price (TOTAL gross sats ÷ amount); the
/// multiply-before-divide keeps precision and the result is clamped to ≥ 1.
pub fn build_mirror_order(
    asset_id: &str,
    filled_side: Side,
    fill_unit_price: u64,
    fill_amount: u64,
    spread_bps: u16,
) -> OrderRecord {
    let mirror_side = opposite(filled_side.clone());
    let factor = match filled_side {
        Side::Buy => 10_000u64.saturating_sub(spread_bps as u64), // buy back cheaper
        Side::Sell => 10_000u64 + spread_bps as u64,              // re-sell dearer
    };
    let mirror_unit = (fill_unit_price.saturating_mul(factor) / 10_000).max(1);
    new_order(
        rfq_store::side_str(&mirror_side),
        asset_id.to_owned(),
        mirror_unit,
        fill_amount,
        true,
        spread_bps,
    )
}

/// One-time import of a legacy `orders.json` (next to the maker config) into the
/// order store. Idempotent: only runs when the store is empty AND the file
/// exists, then renames the file to `orders.json.imported` so it never
/// re-imports. Existing maker.db orders are left untouched.
pub async fn migrate_orders_json(
    store: &dyn OrderStore,
    config_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let json_path = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("orders.json");
    if !json_path.exists() {
        return Ok(());
    }
    if !store.list().await?.is_empty() {
        return Ok(()); // store already populated — don't clobber
    }

    #[derive(serde::Deserialize)]
    struct LegacyBook {
        #[serde(default)]
        orders: Vec<LegacyOrder>,
    }
    #[derive(serde::Deserialize)]
    struct LegacyOrder {
        id: String,
        side: String,
        asset_id: String,
        price: u64,
        size: u64,
        #[serde(default)]
        created_at_ms: u64,
        #[serde(default)]
        mirror: bool,
        #[serde(default)]
        mirror_spread_bps: u16,
    }

    let raw = std::fs::read_to_string(&json_path)?;
    let book: LegacyBook = serde_json::from_str(&raw)?;
    let count = book.orders.len();
    for o in book.orders {
        store
            .upsert(OrderRecord {
                id: o.id,
                side: o.side,
                asset_id: o.asset_id,
                price: o.price,
                size: o.size,
                created_at_ms: o.created_at_ms,
                mirror: o.mirror,
                mirror_spread_bps: o.mirror_spread_bps,
            })
            .await?;
    }
    let _ = std::fs::rename(&json_path, json_path.with_extension("json.imported"));
    if count > 0 {
        println!("migrated {count} order(s) from orders.json into maker.db");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_of_a_sell_order_filled_is_a_dearer_buy() {
        // maker BOUGHT (a `sell` order filled) at unit 100, spread 200bps (2%).
        let m = build_mirror_order("rgb:x", Side::Sell, 100, 500, 200);
        assert_eq!(m.side, "buy");
        assert_eq!(m.price, 102); // 100 * 10200 / 10000
        assert_eq!(m.size, 500);
        assert!(m.mirror);
        assert_eq!(m.mirror_spread_bps, 200);
    }

    #[test]
    fn mirror_of_a_buy_order_filled_is_a_cheaper_sell() {
        // maker SOLD (a `buy` order filled) at unit 100, spread 200bps.
        let m = build_mirror_order("rgb:x", Side::Buy, 100, 500, 200);
        assert_eq!(m.side, "sell");
        assert_eq!(m.price, 98); // 100 * 9800 / 10000
        assert_eq!(m.size, 500);
    }

    #[test]
    fn mirror_price_clamps_to_at_least_one() {
        // Tiny unit price with a large spread would floor to 0 — clamp to 1.
        let m = build_mirror_order("rgb:x", Side::Buy, 1, 10, 9000);
        assert_eq!(m.price, 1);
    }

    #[tokio::test]
    async fn migrate_imports_legacy_orders_json_once() {
        use rfq_store::InMemoryOrderStore;
        let dir = std::env::temp_dir().join(format!("orders-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("maker.toml");
        let json = dir.join("orders.json");
        std::fs::write(
            &json,
            r#"{"orders":[{"id":"ord-x","side":"buy","asset_id":"rgb:x","price":20,"size":1000,"created_at_ms":1}]}"#,
        )
        .unwrap();

        let store = InMemoryOrderStore::new();
        migrate_orders_json(&store, &config_path).await.unwrap();
        let orders = store.list().await.unwrap();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].id, "ord-x");
        // The file is renamed so it never re-imports.
        assert!(!json.exists());
        assert!(dir.join("orders.json.imported").exists());
        // Idempotent: a second call (store already populated) is a no-op.
        migrate_orders_json(&store, &config_path).await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn price_policy_skips_unknown_sides() {
        let orders = vec![
            new_order("buy", "rgb:x".into(), 250, 1000, false, 0),
            OrderRecord {
                id: "b".into(),
                side: "nonsense".into(),
                asset_id: "rgb:x".into(),
                price: 1,
                size: 1,
                created_at_ms: 2,
                mirror: false,
                mirror_spread_bps: 0,
            },
        ];
        let policy = price_policy(&orders);
        assert!(matches!(
            policy.unit_price(
                &rfq_types::AssetId {
                    network: rfq_types::BitcoinNetwork::Regtest,
                    kind: rfq_types::AssetKind::Rgb20,
                    id: "rgb:x".into(),
                },
                &Side::Buy,
                1000,
            ),
            rfq_maker::PriceLookup::Price(250)
        ));
    }
}
