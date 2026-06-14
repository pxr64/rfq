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

use std::collections::HashMap;
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

/// A flat both-sides [`PricePolicy`] for one asset at `price` sats per **token**
/// up to `size`. Convenience for setup/tests that just need the maker to quote an
/// asset (the maker declines any (asset, side) with no order, so a policy is
/// required). Precision is 0 — i.e. token == smallest unit — so `price * amount`
/// is the total, matching the legacy behaviour these helpers were written for.
pub fn flat_policy(asset_id: &str, price: u64, size: u64) -> PricePolicy {
    PricePolicy::from_entries(
        [rfq_types::Side::Buy, rfq_types::Side::Sell]
            .into_iter()
            .map(|side| PriceEntry {
                asset_id: asset_id.to_owned(),
                side,
                price_sats_per_token: price,
                precision: 0,
                max_size: size,
            })
            .collect(),
    )
}

/// Build the maker's [`PricePolicy`] from the orders with a recognized side.
/// `precisions` maps asset id → decimal precision (from the contract registry);
/// an asset missing from the map prices at precision 0.
pub fn price_policy(orders: &[OrderRecord], precisions: &HashMap<String, u8>) -> PricePolicy {
    let entries = orders
        .iter()
        .filter_map(|o| {
            parse_side(&o.side).map(|side| PriceEntry {
                asset_id: o.asset_id.clone(),
                side,
                price_sats_per_token: o.price,
                precision: precisions.get(&o.asset_id).copied().unwrap_or(0),
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
/// standing opposite order plus this fill, priced off the fill's per-unit price
/// by `spread_bps`, and itself mirror-enabled so the loop continues.
///
/// - maker SOLD RGB (a `buy` order filled) → mirror is a `sell` buy-back,
///   **cheaper** (`-spread`).
/// - maker BOUGHT RGB (a `sell` order filled) → mirror is a `buy` re-sell,
///   **dearer** (`+spread`).
///
/// `base_size` is the existing opposite-side order's size (0 if none): the
/// mirror **accumulates** `base_size + fill_amount` so repeated fills grow the
/// buy-back instead of clobbering it down to the last fill. No cap — sizes are
/// re-set out of band.
///
/// `fill_unit_price` is the per-unit price (TOTAL gross sats ÷ amount). Prices
/// are denominated sats-per-smallest-unit, so the per-unit integer is small and
/// a plain `bps` multiply truncates the spread away (a 50 bps move on a price of
/// 101 floors back to 101 — zero edge). We instead round *directionally* away
/// from the fill — floor on the cheaper side, ceil on the dearer side — and force
/// the result strictly past the fill by ≥ 1 unit whenever a spread is configured,
/// so the mirror always carries a nonzero edge. The trade-off (per the chosen
/// fix): on low-priced assets the effective spread is quantized to the per-unit
/// granularity and can overshoot the configured bps. Result is clamped to ≥ 1.
pub fn build_mirror_order(
    asset_id: &str,
    filled_side: Side,
    fill_unit_price: u64,
    fill_amount: u64,
    base_size: u64,
    spread_bps: u16,
) -> OrderRecord {
    let mirror_side = opposite(filled_side.clone());
    let mirror_unit = match &filled_side {
        // maker SOLD RGB → mirror buys it back *cheaper*: floor, forced strictly
        // below the fill price by ≥ 1 (never below 1).
        Side::Buy => {
            let factor = 10_000u64.saturating_sub(spread_bps as u64);
            let target = fill_unit_price.saturating_mul(factor) / 10_000; // floor
            if spread_bps == 0 {
                target.max(1)
            } else {
                target.min(fill_unit_price.saturating_sub(1)).max(1)
            }
        }
        // maker BOUGHT RGB → mirror re-sells it *dearer*: ceil, forced strictly
        // above the fill price by ≥ 1.
        Side::Sell => {
            let factor = 10_000u64 + spread_bps as u64;
            let target = fill_unit_price.saturating_mul(factor).div_ceil(10_000);
            if spread_bps == 0 {
                target.max(1)
            } else {
                target.max(fill_unit_price.saturating_add(1))
            }
        }
    };
    new_order(
        rfq_store::side_str(&mirror_side),
        asset_id.to_owned(),
        mirror_unit,
        base_size.saturating_add(fill_amount),
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
        let m = build_mirror_order("rgb:x", Side::Sell, 100, 500, 0, 200);
        assert_eq!(m.side, "buy");
        assert_eq!(m.price, 102); // 100 * 10200 / 10000
        assert_eq!(m.size, 500);
        assert!(m.mirror);
        assert_eq!(m.mirror_spread_bps, 200);
    }

    #[test]
    fn mirror_of_a_buy_order_filled_is_a_cheaper_sell() {
        // maker SOLD (a `buy` order filled) at unit 100, spread 200bps.
        let m = build_mirror_order("rgb:x", Side::Buy, 100, 500, 0, 200);
        assert_eq!(m.side, "sell");
        assert_eq!(m.price, 98); // 100 * 9800 / 10000
        assert_eq!(m.size, 500);
    }

    #[test]
    fn mirror_price_clamps_to_at_least_one() {
        // Tiny unit price with a large spread would floor to 0 — clamp to 1.
        let m = build_mirror_order("rgb:x", Side::Buy, 1, 10, 0, 9000);
        assert_eq!(m.price, 1);
    }

    #[test]
    fn small_spread_on_low_price_still_moves_by_at_least_one() {
        // Per-smallest-unit price of 101 with a 50bps spread: a plain bps multiply
        // truncates back to 101 (zero edge). Directional rounding must still move.
        let dearer = build_mirror_order("rgb:x", Side::Sell, 101, 10, 0, 50);
        assert_eq!(dearer.side, "buy");
        assert_eq!(dearer.price, 102); // ceil(101 * 10050 / 10000) = ceil(101.5) = 102, > 101
        let cheaper = build_mirror_order("rgb:x", Side::Buy, 101, 10, 0, 50);
        assert_eq!(cheaper.side, "sell");
        assert_eq!(cheaper.price, 100); // floor(101 * 9950 / 10000) = 100, < 101
    }

    #[test]
    fn tiny_spread_is_forced_past_the_fill_price() {
        // 10bps on price 101 rounds back to 101 both ways — force ±1 so there's edge.
        let dearer = build_mirror_order("rgb:x", Side::Sell, 101, 10, 0, 10);
        assert_eq!(dearer.price, 102); // max(ceil(101.101), 101+1) = 102
        let cheaper = build_mirror_order("rgb:x", Side::Buy, 101, 10, 0, 10);
        assert_eq!(cheaper.price, 100); // min(floor(100.899)=100, 101-1=100) = 100
    }

    #[test]
    fn zero_spread_does_not_force_a_move() {
        // No configured spread → no forced ±1; mirror sits at the fill price.
        let dearer = build_mirror_order("rgb:x", Side::Sell, 101, 10, 0, 0);
        assert_eq!(dearer.price, 101);
        let cheaper = build_mirror_order("rgb:x", Side::Buy, 101, 10, 0, 0);
        assert_eq!(cheaper.price, 101);
    }

    #[test]
    fn mirror_size_accumulates_onto_the_standing_opposite_order() {
        // A standing opposite order of 300 plus a 400 fill → 700 (no clobber, no cap).
        let m = build_mirror_order("rgb:x", Side::Buy, 100, 400, 300, 200);
        assert_eq!(m.size, 700);
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
        let policy = price_policy(&orders, &HashMap::new());
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
            rfq_maker::PriceLookup::Price { price_sats_per_token: 250, .. }
        ));
    }
}
