//! Runtime registry of connected makers, keyed by [`MakerId`]. Makers
//! self-register over the WebSocket (`/maker-stream`) and are removed on
//! disconnect. Tests and the in-process mock pre-seed it via
//! [`MakerRegistry::with`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use rfq_router::MakerConnector;
use rfq_types::{AssetInfo, BitcoinNetwork, MakerId, OrderPrice};
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;
use utoipa::ToSchema;

/// How long a pulled price/depth snapshot is served before the next `GET /prices`
/// triggers a fresh pull. Short — the feed is advisory (the binding number is the
/// `POST /rfq` round-trip), so a couple seconds of staleness is fine and it
/// collapses per-keystroke preview spam into one pull per window.
const PRICE_TTL: Duration = Duration::from_secs(2);

/// Per-maker ceiling on a depth pull, so one slow/unreachable maker can't stall
/// the preview. On timeout the maker's last-known prices stand.
const PRICE_PULL_TIMEOUT: Duration = Duration::from_millis(500);

/// A registered maker plus the observability metadata the broker surfaces via
/// `GET /status`. `network`/`assets`/`prices` come from the maker's `Register`
/// frame and are empty for older makers that don't advertise them.
struct Registered {
    connector: Arc<dyn MakerConnector>,
    connected_at: Instant,
    network: Option<BitcoinNetwork>,
    assets: Vec<AssetInfo>,
    prices: Vec<OrderPrice>,
}

/// TTL-cached aggregate for `GET /prices`. Refilled by pulling live depth from
/// connected makers (`MakerConnector::request_prices`) when stale.
#[derive(Default)]
struct PriceCache {
    refreshed_at: Option<Instant>,
    prices: Vec<OrderPrice>,
}

#[derive(Default)]
pub struct MakerRegistry {
    makers: RwLock<HashMap<MakerId, Registered>>,
    price_cache: RwLock<PriceCache>,
    /// Serializes refreshes so concurrent `GET /prices` callers trigger one pull,
    /// not a thundering herd of fan-outs to every maker.
    refresh_lock: Mutex<()>,
}

/// Aggregate broker observability snapshot. `broker_version` is filled in by the
/// HTTP handler (the registry doesn't know the crate version).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BrokerStatus {
    pub makers_online: usize,
    /// Count of distinct RGB contracts served across all makers — each is one
    /// asset pair (asset ↔ BTC).
    pub asset_pairs: usize,
    /// Distinct networks advertised by connected makers.
    pub networks: Vec<BitcoinNetwork>,
    pub makers: Vec<MakerStatus>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MakerStatus {
    pub maker_id: String,
    pub uptime_secs: u64,
    pub network: Option<BitcoinNetwork>,
    /// The RGB contract ids this maker serves.
    pub assets: Vec<String>,
}

impl MakerRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Pre-seed from a static list (the in-process mock maker, or
    /// `HttpMakerConnector`s in the round-trip tests). Sync — used from the
    /// non-async `app*` builders. These carry no advertised metadata
    /// (`network`/`assets` empty).
    pub fn with(makers: Vec<Arc<dyn MakerConnector>>) -> Arc<Self> {
        let map = makers
            .into_iter()
            .map(|m| {
                (
                    m.maker_id(),
                    Registered {
                        connector: m,
                        connected_at: Instant::now(),
                        network: None,
                        assets: Vec::new(),
                        prices: Vec::new(),
                    },
                )
            })
            .collect();
        Arc::new(Self {
            makers: RwLock::new(map),
            price_cache: RwLock::new(PriceCache::default()),
            refresh_lock: Mutex::new(()),
        })
    }

    pub async fn insert(
        &self,
        maker: Arc<dyn MakerConnector>,
        network: Option<BitcoinNetwork>,
        assets: Vec<AssetInfo>,
        prices: Vec<OrderPrice>,
    ) {
        self.makers.write().await.insert(
            maker.maker_id(),
            Registered {
                connected_at: Instant::now(),
                connector: maker,
                network,
                assets,
                prices,
            },
        );
        self.invalidate_prices().await;
    }

    /// Force the next `GET /prices` to re-pull — the maker set changed.
    async fn invalidate_prices(&self) {
        self.price_cache.write().await.refreshed_at = None;
    }

    /// Remove `id` only if the registered connector is *this* exact `Arc` — so a
    /// stale connection's disconnect can't evict a maker that already
    /// reconnected (last-writer-wins under flapping).
    pub async fn remove_if(&self, id: &MakerId, this: &Arc<dyn MakerConnector>) {
        let mut map = self.makers.write().await;
        if matches!(map.get(id), Some(existing) if Arc::ptr_eq(&existing.connector, this)) {
            map.remove(id);
            drop(map);
            self.invalidate_prices().await;
        }
    }

    /// Snapshot of the current connectors (clone the `Arc`s, drop the lock before
    /// awaiting on them in fanout).
    pub async fn snapshot(&self) -> Vec<Arc<dyn MakerConnector>> {
        self.makers
            .read()
            .await
            .values()
            .map(|r| r.connector.clone())
            .collect()
    }

    pub async fn get(&self, id: &MakerId) -> Option<Arc<dyn MakerConnector>> {
        self.makers.read().await.get(id).map(|r| r.connector.clone())
    }

    /// Aggregate observability snapshot for `GET /status`.
    pub async fn status(&self) -> BrokerStatus {
        let map = self.makers.read().await;

        let mut asset_ids: Vec<&str> = Vec::new();
        let mut networks: Vec<BitcoinNetwork> = Vec::new();
        let mut makers: Vec<MakerStatus> = Vec::new();

        for (id, reg) in map.iter() {
            for asset in &reg.assets {
                if !asset_ids.contains(&asset.id.id.as_str()) {
                    asset_ids.push(asset.id.id.as_str());
                }
            }
            if let Some(net) = &reg.network {
                if !networks.contains(net) {
                    networks.push(net.clone());
                }
            }
            makers.push(MakerStatus {
                maker_id: id.0.clone(),
                uptime_secs: reg.connected_at.elapsed().as_secs(),
                network: reg.network.clone(),
                assets: reg.assets.iter().map(|a| a.id.id.clone()).collect(),
            });
        }

        BrokerStatus {
            makers_online: map.len(),
            asset_pairs: asset_ids.len(),
            networks,
            makers,
        }
    }

    /// Distinct assets served across all connected makers, with display
    /// metadata — the broker's asset directory (`GET /assets`). Deduplicated by
    /// contract id (first maker to advertise it wins).
    pub async fn assets(&self) -> Vec<AssetInfo> {
        let map = self.makers.read().await;
        let mut seen: Vec<String> = Vec::new();
        let mut out: Vec<AssetInfo> = Vec::new();
        for reg in map.values() {
            for info in &reg.assets {
                if !seen.contains(&info.id.id) {
                    seen.push(info.id.id.clone());
                    out.push(info.clone());
                }
            }
        }
        out
    }

    /// Best standing-order price per (contract, side) across makers — the
    /// broker's price feed (`GET /prices`). Best for the taker: lowest unit price
    /// on Buy, highest on Sell. Served from a short TTL cache; on a miss it pulls
    /// each maker's live depth on demand (so `available_size` reflects current
    /// inventory) before aggregating.
    pub async fn prices(&self) -> Vec<OrderPrice> {
        if let Some(fresh) = self.cached_prices().await {
            return fresh;
        }
        // One refresh at a time: re-check the cache after taking the lock, since a
        // racing caller may have just refilled it.
        let _guard = self.refresh_lock.lock().await;
        if let Some(fresh) = self.cached_prices().await {
            return fresh;
        }
        self.refresh_prices().await
    }

    /// The cached aggregate, if still within `PRICE_TTL`.
    async fn cached_prices(&self) -> Option<Vec<OrderPrice>> {
        let cache = self.price_cache.read().await;
        match cache.refreshed_at {
            Some(at) if at.elapsed() < PRICE_TTL => Some(cache.prices.clone()),
            _ => None,
        }
    }

    /// Pull live depth from every connected maker, fold it into their last-known
    /// prices, re-aggregate, and cache the result. Caller holds `refresh_lock`.
    async fn refresh_prices(&self) -> Vec<OrderPrice> {
        // Snapshot connectors so the pulls don't hold the makers lock.
        let conns: Vec<(MakerId, Arc<dyn MakerConnector>)> = {
            let map = self.makers.read().await;
            map.iter()
                .map(|(id, r)| (id.clone(), r.connector.clone()))
                .collect()
        };

        // Pull concurrently, each bounded by PRICE_PULL_TIMEOUT. A failed or
        // unsupported pull yields None → that maker's last-known prices stand.
        let results = join_all(conns.into_iter().map(|(id, conn)| async move {
            let pulled = match timeout(PRICE_PULL_TIMEOUT, conn.request_prices()).await {
                Ok(Ok(prices)) => Some(prices),
                _ => None,
            };
            (id, pulled)
        }))
        .await;

        {
            let mut map = self.makers.write().await;
            for (id, pulled) in results {
                if let (Some(reg), Some(prices)) = (map.get_mut(&id), pulled) {
                    reg.prices = prices;
                }
            }
        }

        let aggregate = self.aggregate_prices().await;
        let mut cache = self.price_cache.write().await;
        cache.refreshed_at = Some(Instant::now());
        cache.prices = aggregate.clone();
        aggregate
    }

    /// Best-per-(contract, side) across all makers' last-known prices.
    async fn aggregate_prices(&self) -> Vec<OrderPrice> {
        let map = self.makers.read().await;
        let mut best: Vec<OrderPrice> = Vec::new();
        for reg in map.values() {
            for p in &reg.prices {
                match best
                    .iter_mut()
                    .find(|b| b.contract_id == p.contract_id && b.side == p.side)
                {
                    None => best.push(p.clone()),
                    Some(b) => {
                        let better = match p.side {
                            rfq_types::Side::Buy => p.price_sats_per_token < b.price_sats_per_token,
                            rfq_types::Side::Sell => p.price_sats_per_token > b.price_sats_per_token,
                        };
                        if better {
                            *b = p.clone();
                        }
                    }
                }
            }
        }
        best
    }
}
