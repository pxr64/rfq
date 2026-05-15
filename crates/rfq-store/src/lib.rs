use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use rfq_types::{
    AssetId, ExtendedInventorySnapshot, InventoryError, InventoryStatus, InventoryUtxo, Outpoint,
    Quote, QuoteId, ReservationId, RfqId,
};
use tokio::sync::RwLock;
use uuid::Uuid;

mod btc;
pub use btc::{
    BtcCoinSelectionError, BtcCoinSelector, BtcInventoryStore, BtcSelection,
    GreedyLargestFirstSelector, InMemoryBtcInventoryStore,
};

#[async_trait]
pub trait QuoteStore: Send + Sync {
    async fn save_quote(&self, quote: Quote);

    async fn get_quote(&self, quote_id: &QuoteId) -> Option<Quote>;
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryQuoteStore {
    quotes: Arc<RwLock<HashMap<QuoteId, Quote>>>,
}

impl InMemoryQuoteStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl QuoteStore for InMemoryQuoteStore {
    async fn save_quote(&self, quote: Quote) {
        self.quotes
            .write()
            .await
            .insert(quote.quote_id.clone(), quote);
    }

    async fn get_quote(&self, quote_id: &QuoteId) -> Option<Quote> {
        self.quotes.read().await.get(quote_id).cloned()
    }
}

/// Per-UTXO inventory store. The trait shape admits a SQLite or RocksDB backend
/// in a follow-up; the in-memory impl is the spec for atomicity.
///
/// Mutations that span multiple outpoints (`reserve_utxos`) are all-or-nothing:
/// if any outpoint isn't `Available`, nothing changes. Concurrent callers
/// serialize on a single write lock in the in-memory impl; a SQL backend
/// would map this to a single `BEGIN…COMMIT`.
#[async_trait]
pub trait InventoryStore: Send + Sync {
    /// Replace the entire UTXO set for `asset` with the supplied list. Used
    /// at maker startup after reading from `RgbBackend::list_inventory_utxos`.
    /// UTXOs of other assets are untouched. Idempotent.
    async fn replace_for_asset(
        &self,
        asset: &AssetId,
        utxos: Vec<InventoryUtxo>,
    ) -> Result<(), InventoryError>;

    /// Insert a single change-output UTXO produced by a broadcast tx. Returns
    /// `UtxoNotAvailable` if the outpoint already exists.
    async fn ingest_change_utxo(&self, utxo: InventoryUtxo) -> Result<(), InventoryError>;

    async fn list_for_asset(&self, asset: &AssetId) -> Vec<InventoryUtxo>;

    /// All UTXOs across every asset. Used by the maker for global inventory
    /// summaries that the per-asset surface can't express.
    async fn list_all(&self) -> Vec<InventoryUtxo>;

    /// Subset of `list_for_asset` filtered to `InventoryStatus::Available`.
    /// Input to the coin selector (lands in 14c/14d).
    async fn list_available(&self, asset: &AssetId) -> Vec<InventoryUtxo>;

    async fn get(&self, outpoint: &Outpoint) -> Option<InventoryUtxo>;

    async fn extended_snapshot(&self, asset: &AssetId) -> ExtendedInventorySnapshot;

    /// Locate the active reservation for `quote_id` if one exists. Returns
    /// `None` for unknown / released / already-spent quote ids. Used by
    /// `MockMaker::accept_quote` to bridge the public `Quote` to the internal
    /// `ReservationId`.
    async fn find_reservation_for_quote(&self, quote_id: &QuoteId) -> Option<ReservationId>;

    /// Atomically reserve every supplied outpoint under a fresh reservation
    /// id. Fails with `UtxoNotAvailable` (and mutates nothing) if any outpoint
    /// is missing or already non-`Available`.
    async fn reserve_utxos(
        &self,
        rfq_id: &RfqId,
        quote_id: &QuoteId,
        outpoints: &[Outpoint],
        expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<ReservationId, InventoryError>;

    /// Release a specific reservation back to `Available`. Returns the count
    /// of UTXOs released.
    async fn release_reservation(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Release every reservation whose `expires_at_ms <= now_ms`. Returns the
    /// count of UTXOs released.
    async fn release_expired_reservations(&self, now_ms: u64) -> usize;

    /// Mark every UTXO in `reservation_id` as `Spent`. Accepts UTXOs currently
    /// in `Reserved` or `PendingRgbAcceptance` (the settlement state machine
    /// will drive the latter; today's MockMaker uses the former directly).
    /// Idempotent: a repeat call with the same `reservation_id` succeeds
    /// (no-op) if the UTXOs are already `Spent` under the same witness_txid.
    async fn mark_spent(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Transition every UTXO in `reservation_id` from `Reserved` to
    /// `PendingBitcoinConfirm { witness_txid }`. Used by the settlement
    /// state machine (#9) after the maker broadcasts the witness tx.
    async fn mark_pending_bitcoin_confirm(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Transition every UTXO in `reservation_id` from `PendingBitcoinConfirm`
    /// to `PendingRgbAcceptance`. Used by the settlement state machine (#9)
    /// after the bitcoin tx confirms.
    async fn mark_pending_rgb_acceptance(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Release a reservation back to `Available` because the broadcast itself
    /// failed (mempool rejected, indexer unreachable, etc). Same outcome as
    /// `release_reservation` — distinct method name for observability and
    /// because the settlement state machine (#9) will reach it via a
    /// different state-transition edge than expiry.
    async fn mark_broadcast_failed(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Mark UTXOs in `reservation_id` as `Invalid` because the RGB
    /// consignment was rejected by the counterparty after broadcast.
    /// Input UTXOs cannot be reused — funds are locked behind the witness tx.
    async fn mark_rgb_acceptance_failed(
        &self,
        reservation_id: &ReservationId,
        reason: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Reconcile chain state under a reorg that orphans `witness_txid`:
    /// - Inputs spent into the reorged tx (`Spent { witness_txid: T }`) → `Available`.
    /// - Outputs produced by the reorged tx (`PendingBitcoinConfirm` or
    ///   `PendingRgbAcceptance` referencing T) → `Invalid`.
    ///
    /// Returns the count of UTXOs affected.
    async fn mark_reorged(
        &self,
        witness_txid: &str,
        now_ms: u64,
    ) -> Result<usize, InventoryError>;

    /// Mark a single UTXO as `Invalid` regardless of its current status. Used
    /// for stash inconsistency detected at startup and any other case where
    /// the maker can't safely treat the UTXO as inventory.
    async fn mark_invalid(
        &self,
        outpoint: &Outpoint,
        reason: String,
        now_ms: u64,
    ) -> Result<(), InventoryError>;
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryInventoryStore {
    utxos: Arc<RwLock<HashMap<Outpoint, InventoryUtxo>>>,
}

impl InMemoryInventoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronous seeding helper. Useful in sync constructors (e.g.
    /// `MockMaker::new`) where awaiting `replace_for_asset` would force the
    /// whole constructor to become async.
    pub fn with_seed(utxos: Vec<InventoryUtxo>) -> Self {
        let map: HashMap<Outpoint, InventoryUtxo> =
            utxos.into_iter().map(|u| (u.outpoint.clone(), u)).collect();
        Self {
            utxos: Arc::new(RwLock::new(map)),
        }
    }
}

#[async_trait]
impl InventoryStore for InMemoryInventoryStore {
    async fn replace_for_asset(
        &self,
        asset: &AssetId,
        new_utxos: Vec<InventoryUtxo>,
    ) -> Result<(), InventoryError> {
        let mut utxos = self.utxos.write().await;
        utxos.retain(|_, u| &u.asset_id != asset);
        for utxo in new_utxos {
            utxos.insert(utxo.outpoint.clone(), utxo);
        }
        Ok(())
    }

    async fn ingest_change_utxo(&self, utxo: InventoryUtxo) -> Result<(), InventoryError> {
        let mut utxos = self.utxos.write().await;
        if let Some(existing) = utxos.get(&utxo.outpoint) {
            return Err(InventoryError::UtxoNotAvailable {
                outpoint: utxo.outpoint.clone(),
                status: existing.status.clone(),
            });
        }
        utxos.insert(utxo.outpoint.clone(), utxo);
        Ok(())
    }

    async fn list_for_asset(&self, asset: &AssetId) -> Vec<InventoryUtxo> {
        self.utxos
            .read()
            .await
            .values()
            .filter(|u| &u.asset_id == asset)
            .cloned()
            .collect()
    }

    async fn list_all(&self) -> Vec<InventoryUtxo> {
        self.utxos.read().await.values().cloned().collect()
    }

    async fn list_available(&self, asset: &AssetId) -> Vec<InventoryUtxo> {
        self.utxos
            .read()
            .await
            .values()
            .filter(|u| &u.asset_id == asset && matches!(u.status, InventoryStatus::Available))
            .cloned()
            .collect()
    }

    async fn get(&self, outpoint: &Outpoint) -> Option<InventoryUtxo> {
        self.utxos.read().await.get(outpoint).cloned()
    }

    async fn extended_snapshot(&self, asset: &AssetId) -> ExtendedInventorySnapshot {
        let utxos = self.utxos.read().await;
        ExtendedInventorySnapshot::from_utxos(utxos.values().filter(|u| &u.asset_id == asset))
    }

    async fn find_reservation_for_quote(&self, quote_id: &QuoteId) -> Option<ReservationId> {
        self.utxos.read().await.values().find_map(|u| {
            if let InventoryStatus::Reserved {
                reservation_id,
                quote_id: qid,
                ..
            } = &u.status
            {
                if qid == quote_id {
                    return Some(reservation_id.clone());
                }
            }
            None
        })
    }

    async fn reserve_utxos(
        &self,
        rfq_id: &RfqId,
        quote_id: &QuoteId,
        outpoints: &[Outpoint],
        expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<ReservationId, InventoryError> {
        let mut utxos = self.utxos.write().await;

        // Two-pass: validate all outpoints are Available before mutating any.
        for outpoint in outpoints {
            match utxos.get(outpoint) {
                None => return Err(InventoryError::UtxoNotFound(outpoint.clone())),
                Some(u) if !matches!(u.status, InventoryStatus::Available) => {
                    return Err(InventoryError::UtxoNotAvailable {
                        outpoint: outpoint.clone(),
                        status: u.status.clone(),
                    });
                }
                _ => {}
            }
        }

        let reservation_id = ReservationId(Uuid::new_v4().to_string());
        for outpoint in outpoints {
            let utxo = utxos
                .get_mut(outpoint)
                .expect("checked in validation pass above");
            utxo.status = InventoryStatus::Reserved {
                reservation_id: reservation_id.clone(),
                rfq_id: rfq_id.clone(),
                quote_id: quote_id.clone(),
                expires_at_ms,
            };
            utxo.updated_at_ms = now_ms;
        }
        Ok(reservation_id)
    }

    async fn release_reservation(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut released = 0;
        let mut matched_any = false;

        for utxo in utxos.values_mut() {
            if let InventoryStatus::Reserved {
                reservation_id: rid,
                ..
            } = &utxo.status
            {
                if rid == reservation_id {
                    matched_any = true;
                    utxo.status = InventoryStatus::Available;
                    utxo.updated_at_ms = now_ms;
                    released += 1;
                }
            }
        }

        if !matched_any {
            return Err(InventoryError::ReservationNotFound(reservation_id.clone()));
        }
        Ok(released)
    }

    async fn release_expired_reservations(&self, now_ms: u64) -> usize {
        let mut utxos = self.utxos.write().await;
        let mut released = 0;
        for utxo in utxos.values_mut() {
            let expired = matches!(
                &utxo.status,
                InventoryStatus::Reserved { expires_at_ms, .. } if *expires_at_ms <= now_ms
            );
            if expired {
                utxo.status = InventoryStatus::Available;
                utxo.updated_at_ms = now_ms;
                released += 1;
            }
        }
        released
    }

    async fn mark_spent(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        let mut matched_any = false;
        let mut already_spent = 0;

        for utxo in utxos.values_mut() {
            let (matches, quote_id) = match &utxo.status {
                InventoryStatus::Reserved {
                    reservation_id: rid,
                    quote_id,
                    ..
                } if rid == reservation_id => (true, Some(quote_id.clone())),
                InventoryStatus::PendingRgbAcceptance {
                    reservation_id: rid,
                    ..
                } if rid == reservation_id => {
                    // Quote id is recoverable from the original Reserved state
                    // but not retained on PendingRgbAcceptance. For #9-driven
                    // flows we'd need a side index; #14e callers don't reach
                    // this branch yet, so synthesize a placeholder.
                    (true, None)
                }
                InventoryStatus::Spent {
                    witness_txid: existing_txid,
                    ..
                } if existing_txid == &witness_txid => {
                    already_spent += 1;
                    (false, None)
                }
                _ => (false, None),
            };
            if !matches {
                continue;
            }
            matched_any = true;
            utxo.status = InventoryStatus::Spent {
                witness_txid: witness_txid.clone(),
                quote_id: quote_id
                    .unwrap_or_else(|| QuoteId(format!("recovered-from-{}", reservation_id.0))),
            };
            utxo.updated_at_ms = now_ms;
            updated += 1;
        }

        if !matched_any && already_spent == 0 {
            return Err(InventoryError::ReservationNotFound(reservation_id.clone()));
        }
        Ok(updated)
    }

    async fn mark_pending_bitcoin_confirm(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        for utxo in utxos.values_mut() {
            if let InventoryStatus::Reserved {
                reservation_id: rid,
                ..
            } = &utxo.status
            {
                if rid == reservation_id {
                    utxo.status = InventoryStatus::PendingBitcoinConfirm {
                        reservation_id: reservation_id.clone(),
                        witness_txid: witness_txid.clone(),
                    };
                    utxo.updated_at_ms = now_ms;
                    updated += 1;
                }
            }
        }
        if updated == 0 {
            return Err(InventoryError::ReservationNotFound(reservation_id.clone()));
        }
        Ok(updated)
    }

    async fn mark_pending_rgb_acceptance(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        for utxo in utxos.values_mut() {
            if let InventoryStatus::PendingBitcoinConfirm {
                reservation_id: rid,
                witness_txid,
            } = &utxo.status
            {
                if rid == reservation_id {
                    utxo.status = InventoryStatus::PendingRgbAcceptance {
                        reservation_id: reservation_id.clone(),
                        witness_txid: witness_txid.clone(),
                    };
                    utxo.updated_at_ms = now_ms;
                    updated += 1;
                }
            }
        }
        if updated == 0 {
            return Err(InventoryError::ReservationNotFound(reservation_id.clone()));
        }
        Ok(updated)
    }

    async fn mark_broadcast_failed(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        self.release_reservation(reservation_id, now_ms).await
    }

    async fn mark_rgb_acceptance_failed(
        &self,
        reservation_id: &ReservationId,
        reason: String,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        for utxo in utxos.values_mut() {
            if let InventoryStatus::PendingRgbAcceptance {
                reservation_id: rid,
                ..
            } = &utxo.status
            {
                if rid == reservation_id {
                    utxo.status = InventoryStatus::Invalid {
                        reason: reason.clone(),
                    };
                    utxo.updated_at_ms = now_ms;
                    updated += 1;
                }
            }
        }
        if updated == 0 {
            return Err(InventoryError::ReservationNotFound(reservation_id.clone()));
        }
        Ok(updated)
    }

    async fn mark_reorged(
        &self,
        witness_txid: &str,
        now_ms: u64,
    ) -> Result<usize, InventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        for utxo in utxos.values_mut() {
            let new_status = match &utxo.status {
                InventoryStatus::Spent {
                    witness_txid: wt, ..
                } if wt == witness_txid => Some(InventoryStatus::Available),
                InventoryStatus::PendingBitcoinConfirm {
                    witness_txid: wt, ..
                }
                | InventoryStatus::PendingRgbAcceptance {
                    witness_txid: wt, ..
                } if wt == witness_txid => Some(InventoryStatus::Invalid {
                    reason: format!("reorged: witness tx {witness_txid} orphaned"),
                }),
                _ => None,
            };
            if let Some(status) = new_status {
                utxo.status = status;
                utxo.updated_at_ms = now_ms;
                updated += 1;
            }
        }
        Ok(updated)
    }

    async fn mark_invalid(
        &self,
        outpoint: &Outpoint,
        reason: String,
        now_ms: u64,
    ) -> Result<(), InventoryError> {
        let mut utxos = self.utxos.write().await;
        let utxo = utxos
            .get_mut(outpoint)
            .ok_or_else(|| InventoryError::UtxoNotFound(outpoint.clone()))?;
        utxo.status = InventoryStatus::Invalid { reason };
        utxo.updated_at_ms = now_ms;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_types::{AssetKind, BitcoinNetwork};

    const NOW_MS: u64 = 1_700_000_000_000;
    const TXID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-test".to_owned(),
        }
    }

    fn other_asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-other".to_owned(),
        }
    }

    fn outpoint(vout: u32) -> Outpoint {
        Outpoint::new(TXID, vout)
    }

    fn utxo(asset_id: AssetId, vout: u32, amount: u64) -> InventoryUtxo {
        InventoryUtxo {
            outpoint: outpoint(vout),
            asset_id,
            amount,
            btc_sats: 1000,
            status: InventoryStatus::Available,
            created_at_ms: NOW_MS,
            updated_at_ms: NOW_MS,
            pending_txid: None,
        }
    }

    async fn seeded_store(utxos: Vec<InventoryUtxo>) -> InMemoryInventoryStore {
        let store = InMemoryInventoryStore::new();
        // Group by asset so replace_for_asset doesn't clobber across assets.
        let mut by_asset: HashMap<AssetId, Vec<InventoryUtxo>> = HashMap::new();
        for u in utxos {
            by_asset.entry(u.asset_id.clone()).or_default().push(u);
        }
        for (asset, set) in by_asset {
            store.replace_for_asset(&asset, set).await.unwrap();
        }
        store
    }

    #[tokio::test]
    async fn list_and_get_round_trip_seeded_utxos() {
        let store = seeded_store(vec![
            utxo(asset(), 0, 100),
            utxo(asset(), 1, 200),
            utxo(other_asset(), 2, 50),
        ])
        .await;

        let mine = store.list_for_asset(&asset()).await;
        assert_eq!(mine.len(), 2);

        let got = store.get(&outpoint(0)).await.unwrap();
        assert_eq!(got.amount, 100);
        assert!(store.get(&outpoint(99)).await.is_none());
    }

    #[tokio::test]
    async fn replace_for_asset_keeps_other_assets_intact() {
        let store = seeded_store(vec![
            utxo(asset(), 0, 100),
            utxo(other_asset(), 1, 999),
        ])
        .await;

        store
            .replace_for_asset(&asset(), vec![utxo(asset(), 2, 250)])
            .await
            .unwrap();

        let mine = store.list_for_asset(&asset()).await;
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].amount, 250);

        let other = store.list_for_asset(&other_asset()).await;
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].amount, 999);
    }

    #[tokio::test]
    async fn reserve_utxos_atomically_or_rejects_with_no_mutation() {
        let store = seeded_store(vec![utxo(asset(), 0, 100), utxo(asset(), 1, 200)]).await;

        // Pre-reserve outpoint(1) under a different reservation.
        let _first = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(1)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        // Second call tries to reserve {0, 1}. Should fail and leave 0 untouched.
        let err = store
            .reserve_utxos(
                &RfqId("rfq-2".into()),
                &QuoteId("q-2".into()),
                &[outpoint(0), outpoint(1)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, InventoryError::UtxoNotAvailable { .. }));
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Available
        ));
    }

    #[tokio::test]
    async fn reserve_utxos_rejects_missing_outpoint() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
        let err = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0), outpoint(42)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, InventoryError::UtxoNotFound(_)));
        // outpoint(0) must remain untouched
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Available
        ));
    }

    #[tokio::test]
    async fn release_expired_reservations_releases_only_past_deadlines() {
        let store = seeded_store(vec![utxo(asset(), 0, 100), utxo(asset(), 1, 200)]).await;
        store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS - 1,
                NOW_MS - 2,
            )
            .await
            .unwrap();
        store
            .reserve_utxos(
                &RfqId("rfq-2".into()),
                &QuoteId("q-2".into()),
                &[outpoint(1)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        let released = store.release_expired_reservations(NOW_MS).await;

        assert_eq!(released, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Available
        ));
        assert!(matches!(
            store.get(&outpoint(1)).await.unwrap().status,
            InventoryStatus::Reserved { .. }
        ));
    }

    #[tokio::test]
    async fn release_reservation_targets_specific_reservation_id() {
        let store = seeded_store(vec![utxo(asset(), 0, 100), utxo(asset(), 1, 200)]).await;
        let rid1 = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();
        let _rid2 = store
            .reserve_utxos(
                &RfqId("rfq-2".into()),
                &QuoteId("q-2".into()),
                &[outpoint(1)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        let count = store.release_reservation(&rid1, NOW_MS).await.unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Available
        ));
        assert!(matches!(
            store.get(&outpoint(1)).await.unwrap().status,
            InventoryStatus::Reserved { .. }
        ));
    }

    #[tokio::test]
    async fn release_reservation_unknown_id_errors() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
        let err = store
            .release_reservation(&ReservationId("does-not-exist".into()), NOW_MS)
            .await
            .unwrap_err();
        assert!(matches!(err, InventoryError::ReservationNotFound(_)));
    }

    #[tokio::test]
    async fn mark_spent_transitions_reservation_to_spent() {
        let store = seeded_store(vec![utxo(asset(), 0, 100), utxo(asset(), 1, 200)]).await;
        let rid = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0), outpoint(1)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        let count = store
            .mark_spent(&rid, "wt-1".into(), NOW_MS + 100)
            .await
            .unwrap();
        assert_eq!(count, 2);

        for vout in [0, 1] {
            assert!(matches!(
                store.get(&outpoint(vout)).await.unwrap().status,
                InventoryStatus::Spent { ref witness_txid, .. } if witness_txid == "wt-1"
            ));
        }
    }

    #[tokio::test]
    async fn mark_spent_idempotent_for_same_witness_txid() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
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

        let first = store
            .mark_spent(&rid, "wt-1".into(), NOW_MS + 100)
            .await
            .unwrap();
        let second = store
            .mark_spent(&rid, "wt-1".into(), NOW_MS + 200)
            .await
            .unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0, "idempotent repeat returns 0 updates");
    }

    #[tokio::test]
    async fn ingest_change_utxo_inserts_when_new_and_errors_when_duplicate() {
        let store = seeded_store(vec![]).await;
        let change = utxo(asset(), 5, 42);

        store.ingest_change_utxo(change.clone()).await.unwrap();
        assert_eq!(store.get(&outpoint(5)).await.unwrap().amount, 42);

        let err = store.ingest_change_utxo(change).await.unwrap_err();
        assert!(matches!(err, InventoryError::UtxoNotAvailable { .. }));
    }

    #[tokio::test]
    async fn extended_snapshot_aggregates_per_state() {
        let store = seeded_store(vec![
            utxo(asset(), 0, 100),
            utxo(asset(), 1, 200),
            utxo(asset(), 2, 300),
        ])
        .await;
        // Reserve one, leave two available.
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

        let snap = store.extended_snapshot(&asset()).await;
        assert_eq!(snap.total_amount, 600);
        assert_eq!(snap.available_amount, 500);
        assert_eq!(snap.reserved_amount, 100);
        assert_eq!(snap.total_utxos, 3);
        assert_eq!(snap.available_utxos, 2);
        assert_eq!(snap.reserved_utxos, 1);
        assert_eq!(snap.pending_settlements, 1);
        // largest available is 300 / total available 500 → fragmentation 0.4
        assert!((snap.fragmentation_score - 0.4).abs() < 1e-9);
    }

    #[tokio::test]
    async fn fragmentation_score_is_zero_for_single_available_utxo() {
        let store = seeded_store(vec![utxo(asset(), 0, 1000)]).await;
        let snap = store.extended_snapshot(&asset()).await;
        assert_eq!(snap.fragmentation_score, 0.0);
    }

    #[tokio::test]
    async fn fragmentation_score_zero_when_no_available_amount() {
        let store = seeded_store(vec![]).await;
        let snap = store.extended_snapshot(&asset()).await;
        assert_eq!(snap.fragmentation_score, 0.0);
        assert_eq!(snap.total_amount, 0);
    }

    // --- 14e failure-handling tests ---

    #[tokio::test]
    async fn mark_pending_bitcoin_confirm_transitions_reserved_to_pending() {
        let store = seeded_store(vec![utxo(asset(), 0, 100), utxo(asset(), 1, 200)]).await;
        let rid = store
            .reserve_utxos(
                &RfqId("rfq-1".into()),
                &QuoteId("q-1".into()),
                &[outpoint(0), outpoint(1)],
                NOW_MS + 30_000,
                NOW_MS,
            )
            .await
            .unwrap();

        let count = store
            .mark_pending_bitcoin_confirm(&rid, "wt-1".into(), NOW_MS + 100)
            .await
            .unwrap();
        assert_eq!(count, 2);

        for vout in [0, 1] {
            let status = store.get(&outpoint(vout)).await.unwrap().status;
            match status {
                InventoryStatus::PendingBitcoinConfirm {
                    reservation_id,
                    witness_txid,
                } => {
                    assert_eq!(reservation_id, rid);
                    assert_eq!(witness_txid, "wt-1");
                }
                other => panic!("expected PendingBitcoinConfirm, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn mark_pending_rgb_acceptance_transitions_pending_btc_to_pending_rgb() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
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

        let count = store
            .mark_pending_rgb_acceptance(&rid, NOW_MS + 200)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::PendingRgbAcceptance { ref witness_txid, .. } if witness_txid == "wt-1"
        ));
    }

    #[tokio::test]
    async fn mark_spent_accepts_pending_rgb_acceptance_input() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
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
        store
            .mark_pending_rgb_acceptance(&rid, NOW_MS + 200)
            .await
            .unwrap();

        let count = store
            .mark_spent(&rid, "wt-1".into(), NOW_MS + 300)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Spent { ref witness_txid, .. } if witness_txid == "wt-1"
        ));
    }

    #[tokio::test]
    async fn mark_broadcast_failed_returns_to_available() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
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

        let count = store
            .mark_broadcast_failed(&rid, NOW_MS + 100)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Available
        ));
    }

    #[tokio::test]
    async fn mark_rgb_acceptance_failed_transitions_to_invalid_with_reason() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
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
        store
            .mark_pending_rgb_acceptance(&rid, NOW_MS + 200)
            .await
            .unwrap();

        let count = store
            .mark_rgb_acceptance_failed(&rid, "counterparty rejected".into(), NOW_MS + 300)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Invalid { ref reason } if reason == "counterparty rejected"
        ));
    }

    #[tokio::test]
    async fn mark_reorged_releases_spent_inputs_and_invalidates_pending_outputs() {
        let store = seeded_store(vec![
            utxo(asset(), 0, 100),
            utxo(asset(), 1, 50),
            utxo(asset(), 2, 200),
        ])
        .await;
        // UTXO 0: spent in the reorged tx → should go back to Available.
        let rid_spent = store
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
            .mark_spent(&rid_spent, "wt-reorged".into(), NOW_MS + 100)
            .await
            .unwrap();
        // UTXO 1: an output (change-style) of the reorged tx that was awaiting
        // bitcoin confirmation → should go to Invalid.
        store
            .ingest_change_utxo(InventoryUtxo {
                status: InventoryStatus::PendingBitcoinConfirm {
                    reservation_id: ReservationId("rid-x".into()),
                    witness_txid: "wt-reorged".into(),
                },
                pending_txid: Some("wt-reorged".into()),
                ..utxo(asset(), 5, 25)
            })
            .await
            .unwrap();
        // UTXO 2: unrelated; should not be touched.

        let updated = store.mark_reorged("wt-reorged", NOW_MS + 200).await.unwrap();
        assert_eq!(updated, 2);

        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Available
        ));
        assert!(matches!(
            store.get(&outpoint(5)).await.unwrap().status,
            InventoryStatus::Invalid { .. }
        ));
        assert!(matches!(
            store.get(&outpoint(2)).await.unwrap().status,
            InventoryStatus::Available
        ));
    }

    #[tokio::test]
    async fn mark_invalid_sets_status_regardless_of_prior_state() {
        let store = seeded_store(vec![utxo(asset(), 0, 100)]).await;
        store
            .mark_invalid(&outpoint(0), "stash drift".into(), NOW_MS + 100)
            .await
            .unwrap();
        assert!(matches!(
            store.get(&outpoint(0)).await.unwrap().status,
            InventoryStatus::Invalid { ref reason } if reason == "stash drift"
        ));
    }

    #[tokio::test]
    async fn mark_invalid_unknown_outpoint_errors() {
        let store = seeded_store(vec![]).await;
        let err = store
            .mark_invalid(&outpoint(99), "x".into(), NOW_MS)
            .await
            .unwrap_err();
        assert!(matches!(err, InventoryError::UtxoNotFound(_)));
    }

    #[tokio::test]
    async fn legacy_snapshot_view_derives_from_extended_snapshot() {
        let store = seeded_store(vec![
            utxo(asset(), 0, 100),
            utxo(asset(), 1, 200),
        ])
        .await;
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

        let ext = store.extended_snapshot(&asset()).await;
        let legacy: rfq_types::InventorySnapshot = (&ext).into();
        assert_eq!(legacy.total_amount, 300);
        assert_eq!(legacy.available_amount, 200);
        assert_eq!(legacy.reserved_amount, 100);
        assert_eq!(legacy.spent_amount, 0);
        assert_eq!(legacy.total_allocations, 2);
        assert_eq!(legacy.available_allocations, 1);
        assert_eq!(legacy.reserved_allocations, 1);
        assert_eq!(legacy.spent_allocations, 0);
    }
}
