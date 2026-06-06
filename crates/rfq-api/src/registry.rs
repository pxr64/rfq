//! Runtime registry of connected makers, keyed by [`MakerId`]. Replaces the
//! startup-fixed `Vec<Arc<dyn MakerConnector>>`: makers self-register over the
//! WebSocket (`/maker-stream`) and are removed on disconnect. The static
//! `BROKER_MAKER` path pre-seeds it via [`MakerRegistry::with`].

use std::collections::HashMap;
use std::sync::Arc;

use rfq_router::MakerConnector;
use rfq_types::MakerId;
use tokio::sync::RwLock;

#[derive(Default)]
pub struct MakerRegistry {
    makers: RwLock<HashMap<MakerId, Arc<dyn MakerConnector>>>,
}

impl MakerRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Pre-seed from a static list (e.g. `BROKER_MAKER` HTTP connectors or the
    /// in-process mock maker). Sync — used from the non-async `app*` builders.
    pub fn with(makers: Vec<Arc<dyn MakerConnector>>) -> Arc<Self> {
        let map = makers.into_iter().map(|m| (m.maker_id(), m)).collect();
        Arc::new(Self {
            makers: RwLock::new(map),
        })
    }

    pub async fn insert(&self, maker: Arc<dyn MakerConnector>) {
        self.makers.write().await.insert(maker.maker_id(), maker);
    }

    /// Remove `id` only if the registered connector is *this* exact `Arc` — so a
    /// stale connection's disconnect can't evict a maker that already
    /// reconnected (last-writer-wins under flapping).
    pub async fn remove_if(&self, id: &MakerId, this: &Arc<dyn MakerConnector>) {
        let mut map = self.makers.write().await;
        if matches!(map.get(id), Some(existing) if Arc::ptr_eq(existing, this)) {
            map.remove(id);
        }
    }

    /// Snapshot of the current connectors (clone the `Arc`s, drop the lock before
    /// awaiting on them in fanout).
    pub async fn snapshot(&self) -> Vec<Arc<dyn MakerConnector>> {
        self.makers.read().await.values().cloned().collect()
    }

    pub async fn get(&self, id: &MakerId) -> Option<Arc<dyn MakerConnector>> {
        self.makers.read().await.get(id).cloned()
    }
}
