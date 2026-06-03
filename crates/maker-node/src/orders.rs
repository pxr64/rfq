//! Standing maker orders: persisted quote-offers that set the price the maker
//! quotes per (asset, side), replacing the flat default markup. Managed via
//! `colorex maker order {create,list,cancel}` and loaded by `maker up` into the
//! maker's [`PricePolicy`](rfq_maker::PricePolicy).
//!
//! Persistence is a small JSON file (`orders.json`) next to the maker config,
//! so the operator's liquidity terms survive restarts and are editable
//! out-of-band. At most one order is kept per (asset, side) — creating a second
//! upserts the first (a maker quotes one standing price per side, per asset).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rfq_maker::{PriceEntry, PricePolicy};
use rfq_types::Side;
use serde::{Deserialize, Serialize};

/// One standing order. `price` is sats per smallest RGB unit; `size` is the
/// largest single quote (in smallest RGB units) it backs — a request above it
/// is declined rather than quoted at the flat fallback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Order {
    pub id: String,
    /// `"buy"` (taker buys RGB) or `"sell"` (taker sells RGB).
    pub side: String,
    pub asset_id: String,
    pub price: u64,
    pub size: u64,
    pub created_at_ms: u64,
}

/// The on-disk set of standing orders.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrderBook {
    #[serde(default)]
    pub orders: Vec<Order>,
}

impl OrderBook {
    /// `orders.json` next to the maker config file.
    pub fn path_for(config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("orders.json")
    }

    /// Load the order book, treating a missing file as empty.
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Ok(serde_json::from_str(&raw)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Insert an order, replacing any existing one for the same (asset, side).
    /// Returns the id of any order that was replaced.
    pub fn upsert(&mut self, order: Order) -> Option<String> {
        let replaced = self
            .orders
            .iter()
            .position(|o| o.asset_id == order.asset_id && o.side == order.side)
            .map(|i| self.orders.remove(i).id);
        self.orders.push(order);
        replaced
    }

    /// Remove the order with `id`. Returns true if one was removed.
    pub fn cancel(&mut self, id: &str) -> bool {
        let before = self.orders.len();
        self.orders.retain(|o| o.id != id);
        self.orders.len() != before
    }

    /// Build the maker's [`PricePolicy`] from the orders with a recognized side.
    pub fn price_policy(&self) -> PricePolicy {
        let entries = self
            .orders
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
}

/// Parse a side label (case-insensitive) into a [`Side`].
pub fn parse_side(s: &str) -> Option<Side> {
    match s.to_ascii_lowercase().as_str() {
        "buy" => Some(Side::Buy),
        "sell" => Some(Side::Sell),
        _ => None,
    }
}

/// Milliseconds since the Unix epoch — used for order ids + timestamps.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Construct an order with a generated id (hex of the creation timestamp).
pub fn new_order(side: &str, asset_id: String, price: u64, size: u64) -> Order {
    let created_at_ms = now_ms();
    Order {
        id: format!("ord-{created_at_ms:x}"),
        side: side.to_ascii_lowercase(),
        asset_id,
        price,
        size,
        created_at_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_replaces_same_asset_side() {
        let mut book = OrderBook::default();
        assert!(book
            .upsert(Order {
                id: "a".into(),
                side: "buy".into(),
                asset_id: "rgb:x".into(),
                price: 100,
                size: 10,
                created_at_ms: 1,
            })
            .is_none());
        // Same (asset, side) → replaces, returns the old id.
        let replaced = book.upsert(Order {
            id: "b".into(),
            side: "buy".into(),
            asset_id: "rgb:x".into(),
            price: 200,
            size: 20,
            created_at_ms: 2,
        });
        assert_eq!(replaced.as_deref(), Some("a"));
        assert_eq!(book.orders.len(), 1);
        assert_eq!(book.orders[0].price, 200);
        // Different side → coexists.
        book.upsert(Order {
            id: "c".into(),
            side: "sell".into(),
            asset_id: "rgb:x".into(),
            price: 90,
            size: 5,
            created_at_ms: 3,
        });
        assert_eq!(book.orders.len(), 2);
    }

    #[test]
    fn price_policy_skips_unknown_sides() {
        let book = OrderBook {
            orders: vec![
                Order {
                    id: "a".into(),
                    side: "buy".into(),
                    asset_id: "rgb:x".into(),
                    price: 250,
                    size: 1000,
                    created_at_ms: 1,
                },
                Order {
                    id: "b".into(),
                    side: "nonsense".into(),
                    asset_id: "rgb:x".into(),
                    price: 1,
                    size: 1,
                    created_at_ms: 2,
                },
            ],
        };
        // Only the valid order becomes a price entry.
        let policy = book.price_policy();
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
