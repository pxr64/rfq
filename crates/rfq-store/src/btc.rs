//! Maker BTC inventory store (issue #20 / 16a).
//!
//! Parallel to [`crate::InventoryStore`] but for plain (non-RGB) BTC UTXOs the
//! maker uses to pay out on the sell-side swap. The shape mirrors the RGB store
//! deliberately so the two are maintained side by side; the differences are:
//!
//! - no per-asset split (BTC is one fungible pool) — `replace_all` not
//!   `replace_for_asset`;
//! - no `PendingRgbAcceptance` stage — a plain BTC payout is final once the
//!   witness tx confirms, so the lifecycle is just
//!   `Available → Reserved → PendingBitcoinConfirm → Spent`;
//! - coin selection is greedy-largest-first (no denomination targeting — BTC
//!   change just goes back to the maker), so the selector lives here next to
//!   the store rather than in `rfq-maker` where the denomination-aware RGB
//!   `CoinSelector` evolved.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use rfq_types::{
    BtcInventoryError, BtcInventorySnapshot, BtcInventoryStatus, BtcInventoryUtxo, Outpoint,
    QuoteId, ReservationId, RfqId,
};
use tokio::sync::RwLock;
use uuid::Uuid;

/// Maker BTC UTXO store. Multi-outpoint mutations (`reserve`) are
/// all-or-nothing; concurrent callers serialize on a single write lock.
#[async_trait]
pub trait BtcInventoryStore: Send + Sync {
    /// Replace the entire BTC UTXO set. Used at maker startup after the
    /// bitcoin-side bootstrap resolves the maker's funding outpoints.
    async fn replace_all(&self, utxos: Vec<BtcInventoryUtxo>);

    /// Insert a single change-output UTXO produced by a broadcast tx. Returns
    /// `UtxoNotAvailable` if the outpoint already exists.
    async fn ingest_change_utxo(&self, utxo: BtcInventoryUtxo)
        -> Result<(), BtcInventoryError>;

    async fn list_all(&self) -> Vec<BtcInventoryUtxo>;

    /// Subset of `list_all` filtered to `BtcInventoryStatus::Available`. Input
    /// to the coin selector.
    async fn list_available(&self) -> Vec<BtcInventoryUtxo>;

    async fn get(&self, outpoint: &Outpoint) -> Option<BtcInventoryUtxo>;

    async fn snapshot(&self) -> BtcInventorySnapshot;

    /// Locate the active reservation for `quote_id` if one exists.
    async fn find_reservation_for_quote(&self, quote_id: &QuoteId) -> Option<ReservationId>;

    /// Atomically reserve every supplied outpoint under a fresh reservation id.
    /// Fails with `UtxoNotAvailable` / `UtxoNotFound` (and mutates nothing) if
    /// any outpoint is missing or already non-`Available`.
    async fn reserve(
        &self,
        rfq_id: &RfqId,
        quote_id: &QuoteId,
        outpoints: &[Outpoint],
        expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<ReservationId, BtcInventoryError>;

    /// Push out the deadline of an active reservation. Used as the sell-side
    /// settlement advances through its stages: the quote-stage reservation is
    /// extended at `accept` (to the consignment window) and again at
    /// `/consignment` (to the taker-signature window), so the cleanup loop
    /// doesn't release BTC out from under an in-flight swap. Returns the count
    /// of UTXOs whose deadline moved. Mirrors `InventoryStore::extend_reservation`.
    async fn extend_reservation(
        &self,
        reservation_id: &ReservationId,
        new_expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError>;

    /// Release a specific reservation back to `Available`. Returns the count of
    /// UTXOs released.
    async fn release_reservation(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError>;

    /// Release every reservation whose `expires_at_ms <= now_ms`. Returns the
    /// count of UTXOs released.
    async fn release_expired_reservations(&self, now_ms: u64) -> usize;

    /// Transition every UTXO in `reservation_id` from `Reserved` to
    /// `PendingBitcoinConfirm { witness_txid }`. Used after the maker
    /// broadcasts the swap witness tx.
    async fn mark_pending_bitcoin_confirm(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError>;

    /// Mark every UTXO in `reservation_id` as `Spent`. Accepts UTXOs currently
    /// `Reserved` or `PendingBitcoinConfirm`. Idempotent: a repeat call with
    /// the same `reservation_id` + `witness_txid` is a no-op success.
    async fn mark_spent(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError>;

    /// Release a reservation back to `Available` because the broadcast failed.
    /// Same outcome as `release_reservation` — distinct name for observability
    /// and because the settlement state machine (#9) reaches it via a
    /// different edge.
    async fn mark_broadcast_failed(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError>;

    /// Reconcile chain state under a reorg that orphans `witness_txid`:
    /// - inputs spent into the reorged tx (`Spent { witness_txid: T }`) → `Available`;
    /// - outputs produced by the reorged tx (`PendingBitcoinConfirm` referencing
    ///   T) → `Invalid`.
    ///
    /// Returns the count of UTXOs affected.
    async fn mark_reorged(
        &self,
        witness_txid: &str,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError>;

    /// Mark a single UTXO `Invalid` regardless of its current status.
    async fn mark_invalid(
        &self,
        outpoint: &Outpoint,
        reason: String,
        now_ms: u64,
    ) -> Result<(), BtcInventoryError>;
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryBtcInventoryStore {
    utxos: Arc<RwLock<HashMap<Outpoint, BtcInventoryUtxo>>>,
}

impl InMemoryBtcInventoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronous seeding helper for sync constructors / tests.
    pub fn with_seed(utxos: Vec<BtcInventoryUtxo>) -> Self {
        let map: HashMap<Outpoint, BtcInventoryUtxo> =
            utxos.into_iter().map(|u| (u.outpoint.clone(), u)).collect();
        Self {
            utxos: Arc::new(RwLock::new(map)),
        }
    }
}

#[async_trait]
impl BtcInventoryStore for InMemoryBtcInventoryStore {
    async fn replace_all(&self, new_utxos: Vec<BtcInventoryUtxo>) {
        let mut utxos = self.utxos.write().await;
        utxos.clear();
        for utxo in new_utxos {
            utxos.insert(utxo.outpoint.clone(), utxo);
        }
    }

    async fn ingest_change_utxo(
        &self,
        utxo: BtcInventoryUtxo,
    ) -> Result<(), BtcInventoryError> {
        let mut utxos = self.utxos.write().await;
        if let Some(existing) = utxos.get(&utxo.outpoint) {
            return Err(BtcInventoryError::UtxoNotAvailable {
                outpoint: utxo.outpoint.clone(),
                status: existing.status.clone(),
            });
        }
        utxos.insert(utxo.outpoint.clone(), utxo);
        Ok(())
    }

    async fn list_all(&self) -> Vec<BtcInventoryUtxo> {
        self.utxos.read().await.values().cloned().collect()
    }

    async fn list_available(&self) -> Vec<BtcInventoryUtxo> {
        self.utxos
            .read()
            .await
            .values()
            .filter(|u| matches!(u.status, BtcInventoryStatus::Available))
            .cloned()
            .collect()
    }

    async fn get(&self, outpoint: &Outpoint) -> Option<BtcInventoryUtxo> {
        self.utxos.read().await.get(outpoint).cloned()
    }

    async fn snapshot(&self) -> BtcInventorySnapshot {
        let utxos = self.utxos.read().await;
        BtcInventorySnapshot::from_utxos(utxos.values())
    }

    async fn find_reservation_for_quote(&self, quote_id: &QuoteId) -> Option<ReservationId> {
        self.utxos.read().await.values().find_map(|u| {
            if let BtcInventoryStatus::Reserved {
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

    async fn reserve(
        &self,
        rfq_id: &RfqId,
        quote_id: &QuoteId,
        outpoints: &[Outpoint],
        expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<ReservationId, BtcInventoryError> {
        let mut utxos = self.utxos.write().await;

        // Two-pass: validate every outpoint is Available before mutating any.
        for outpoint in outpoints {
            match utxos.get(outpoint) {
                None => return Err(BtcInventoryError::UtxoNotFound(outpoint.clone())),
                Some(u) if !matches!(u.status, BtcInventoryStatus::Available) => {
                    return Err(BtcInventoryError::UtxoNotAvailable {
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
            utxo.status = BtcInventoryStatus::Reserved {
                reservation_id: reservation_id.clone(),
                rfq_id: rfq_id.clone(),
                quote_id: quote_id.clone(),
                expires_at_ms,
            };
            utxo.updated_at_ms = now_ms;
        }
        Ok(reservation_id)
    }

    async fn extend_reservation(
        &self,
        reservation_id: &ReservationId,
        new_expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        for utxo in utxos.values_mut() {
            if let BtcInventoryStatus::Reserved {
                reservation_id: rid,
                rfq_id,
                quote_id,
                ..
            } = &utxo.status
            {
                if rid == reservation_id {
                    utxo.status = BtcInventoryStatus::Reserved {
                        reservation_id: rid.clone(),
                        rfq_id: rfq_id.clone(),
                        quote_id: quote_id.clone(),
                        expires_at_ms: new_expires_at_ms,
                    };
                    utxo.updated_at_ms = now_ms;
                    updated += 1;
                }
            }
        }
        if updated == 0 {
            return Err(BtcInventoryError::ReservationNotFound(
                reservation_id.clone(),
            ));
        }
        Ok(updated)
    }

    async fn release_reservation(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut released = 0;
        let mut matched_any = false;

        for utxo in utxos.values_mut() {
            if let BtcInventoryStatus::Reserved {
                reservation_id: rid,
                ..
            } = &utxo.status
            {
                if rid == reservation_id {
                    matched_any = true;
                    utxo.status = BtcInventoryStatus::Available;
                    utxo.updated_at_ms = now_ms;
                    released += 1;
                }
            }
        }

        if !matched_any {
            return Err(BtcInventoryError::ReservationNotFound(
                reservation_id.clone(),
            ));
        }
        Ok(released)
    }

    async fn release_expired_reservations(&self, now_ms: u64) -> usize {
        let mut utxos = self.utxos.write().await;
        let mut released = 0;
        for utxo in utxos.values_mut() {
            let expired = matches!(
                &utxo.status,
                BtcInventoryStatus::Reserved { expires_at_ms, .. } if *expires_at_ms <= now_ms
            );
            if expired {
                utxo.status = BtcInventoryStatus::Available;
                utxo.updated_at_ms = now_ms;
                released += 1;
            }
        }
        released
    }

    async fn mark_pending_bitcoin_confirm(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        for utxo in utxos.values_mut() {
            if let BtcInventoryStatus::Reserved {
                reservation_id: rid,
                ..
            } = &utxo.status
            {
                if rid == reservation_id {
                    utxo.status = BtcInventoryStatus::PendingBitcoinConfirm {
                        reservation_id: reservation_id.clone(),
                        witness_txid: witness_txid.clone(),
                    };
                    utxo.updated_at_ms = now_ms;
                    updated += 1;
                }
            }
        }
        if updated == 0 {
            return Err(BtcInventoryError::ReservationNotFound(
                reservation_id.clone(),
            ));
        }
        Ok(updated)
    }

    async fn mark_spent(
        &self,
        reservation_id: &ReservationId,
        witness_txid: String,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        let mut matched_any = false;
        let mut already_spent = 0;

        for utxo in utxos.values_mut() {
            let (matches, quote_id) = match &utxo.status {
                BtcInventoryStatus::Reserved {
                    reservation_id: rid,
                    quote_id,
                    ..
                } if rid == reservation_id => (true, Some(quote_id.clone())),
                BtcInventoryStatus::PendingBitcoinConfirm {
                    reservation_id: rid,
                    ..
                } if rid == reservation_id => {
                    // quote_id isn't retained on PendingBitcoinConfirm; mirror
                    // the RGB store's placeholder-synthesis quirk rather than
                    // widening the variant.
                    (true, None)
                }
                BtcInventoryStatus::Spent {
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
            utxo.status = BtcInventoryStatus::Spent {
                witness_txid: witness_txid.clone(),
                quote_id: quote_id
                    .unwrap_or_else(|| QuoteId(format!("recovered-from-{}", reservation_id.0))),
            };
            utxo.updated_at_ms = now_ms;
            updated += 1;
        }

        if !matched_any && already_spent == 0 {
            return Err(BtcInventoryError::ReservationNotFound(
                reservation_id.clone(),
            ));
        }
        Ok(updated)
    }

    async fn mark_broadcast_failed(
        &self,
        reservation_id: &ReservationId,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError> {
        self.release_reservation(reservation_id, now_ms).await
    }

    async fn mark_reorged(
        &self,
        witness_txid: &str,
        now_ms: u64,
    ) -> Result<usize, BtcInventoryError> {
        let mut utxos = self.utxos.write().await;
        let mut updated = 0;
        for utxo in utxos.values_mut() {
            let new_status = match &utxo.status {
                BtcInventoryStatus::Spent {
                    witness_txid: wt, ..
                } if wt == witness_txid => Some(BtcInventoryStatus::Available),
                BtcInventoryStatus::PendingBitcoinConfirm {
                    witness_txid: wt, ..
                } if wt == witness_txid => Some(BtcInventoryStatus::Invalid {
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
    ) -> Result<(), BtcInventoryError> {
        let mut utxos = self.utxos.write().await;
        let utxo = utxos
            .get_mut(outpoint)
            .ok_or_else(|| BtcInventoryError::UtxoNotFound(outpoint.clone()))?;
        utxo.status = BtcInventoryStatus::Invalid { reason };
        utxo.updated_at_ms = now_ms;
        Ok(())
    }
}

// --- Coin selection ---

/// Result of a successful BTC coin selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtcSelection {
    pub chosen: Vec<Outpoint>,
    pub requested_sats: u64,
    pub total_chosen_sats: u64,
}

impl BtcSelection {
    /// Sats over `requested_sats` — the maker's BTC change output.
    pub fn change_sats(&self) -> u64 {
        self.total_chosen_sats.saturating_sub(self.requested_sats)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BtcCoinSelectionError {
    /// Available UTXOs sum to less than `requested`.
    Insufficient { requested: u64, available: u64 },
}

impl std::fmt::Display for BtcCoinSelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Insufficient {
                requested,
                available,
            } => write!(
                f,
                "insufficient btc for coin selection: requested {requested} sats, \
                 available {available} sats"
            ),
        }
    }
}

impl std::error::Error for BtcCoinSelectionError {}

pub trait BtcCoinSelector: Send + Sync {
    /// Pick a set of UTXOs whose values sum to at least `requested_sats`.
    fn select(
        &self,
        requested_sats: u64,
        available: &[BtcInventoryUtxo],
    ) -> Result<BtcSelection, BtcCoinSelectionError>;
}

/// Greedy largest-first: sort available descending by value, accumulate until
/// the running sum covers the request. Minimizes input count (hence vbytes /
/// fee); BTC change just returns to the maker, so there's no denomination
/// target to optimize for — unlike the RGB `GreedyExactFitSelector`.
pub struct GreedyLargestFirstSelector;

impl BtcCoinSelector for GreedyLargestFirstSelector {
    fn select(
        &self,
        requested_sats: u64,
        available: &[BtcInventoryUtxo],
    ) -> Result<BtcSelection, BtcCoinSelectionError> {
        let total_available: u64 = available.iter().map(|u| u.value_sats).sum();
        if total_available < requested_sats {
            return Err(BtcCoinSelectionError::Insufficient {
                requested: requested_sats,
                available: total_available,
            });
        }

        let mut sorted: Vec<&BtcInventoryUtxo> = available.iter().collect();
        // Largest first; tie-break on outpoint for determinism.
        sorted.sort_by(|a, b| {
            b.value_sats
                .cmp(&a.value_sats)
                .then_with(|| a.outpoint.cmp(&b.outpoint))
        });

        let mut chosen = Vec::new();
        let mut total_chosen_sats = 0u64;
        for utxo in sorted {
            if total_chosen_sats >= requested_sats {
                break;
            }
            chosen.push(utxo.outpoint.clone());
            total_chosen_sats = total_chosen_sats.saturating_add(utxo.value_sats);
        }

        Ok(BtcSelection {
            chosen,
            requested_sats,
            total_chosen_sats,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(idx: u32) -> Outpoint {
        Outpoint::new(format!("{idx:064x}"), 0)
    }

    fn utxo(idx: u32, value_sats: u64, status: BtcInventoryStatus) -> BtcInventoryUtxo {
        BtcInventoryUtxo {
            outpoint: op(idx),
            value_sats,
            script_pubkey: vec![0x00, 0x14],
            status,
            created_at_ms: 0,
            updated_at_ms: 0,
            pending_txid: None,
        }
    }

    fn available(idx: u32, value_sats: u64) -> BtcInventoryUtxo {
        utxo(idx, value_sats, BtcInventoryStatus::Available)
    }

    fn rfq() -> RfqId {
        RfqId("rfq-1".to_owned())
    }

    fn quote(id: &str) -> QuoteId {
        QuoteId(id.to_owned())
    }

    #[tokio::test]
    async fn reserve_then_snapshot_moves_sats_to_reserved() {
        let store = InMemoryBtcInventoryStore::with_seed(vec![
            available(0, 100_000),
            available(1, 50_000),
        ]);

        let res = store
            .reserve(&rfq(), &quote("q1"), &[op(0)], 1_000, 0)
            .await
            .unwrap();
        assert!(!res.0.is_empty());

        let snap = store.snapshot().await;
        assert_eq!(snap.total_sats, 150_000);
        assert_eq!(snap.available_sats, 50_000);
        assert_eq!(snap.reserved_sats, 100_000);
        assert_eq!(snap.available_utxos, 1);
        assert_eq!(snap.reserved_utxos, 1);
    }

    #[tokio::test]
    async fn reserve_is_all_or_nothing() {
        let store = InMemoryBtcInventoryStore::with_seed(vec![available(0, 100_000)]);

        // op(0) is available, op(9) doesn't exist — the whole reserve fails
        // and op(0) stays Available.
        let result = store
            .reserve(&rfq(), &quote("q1"), &[op(0), op(9)], 1_000, 0)
            .await;
        assert!(matches!(
            result,
            Err(BtcInventoryError::UtxoNotFound(_))
        ));

        let snap = store.snapshot().await;
        assert_eq!(snap.available_utxos, 1);
        assert_eq!(snap.reserved_utxos, 0);
    }

    #[tokio::test]
    async fn double_reserve_of_same_utxo_is_rejected() {
        let store = InMemoryBtcInventoryStore::with_seed(vec![available(0, 100_000)]);
        store
            .reserve(&rfq(), &quote("q1"), &[op(0)], 1_000, 0)
            .await
            .unwrap();

        let second = store
            .reserve(&rfq(), &quote("q2"), &[op(0)], 1_000, 0)
            .await;
        assert!(matches!(
            second,
            Err(BtcInventoryError::UtxoNotAvailable { .. })
        ));
    }

    #[tokio::test]
    async fn release_expired_reservations_frees_only_expired() {
        let store = InMemoryBtcInventoryStore::with_seed(vec![
            available(0, 100_000),
            available(1, 100_000),
        ]);
        // q1 expires at 500, q2 at 5000.
        store
            .reserve(&rfq(), &quote("q1"), &[op(0)], 500, 0)
            .await
            .unwrap();
        store
            .reserve(&rfq(), &quote("q2"), &[op(1)], 5_000, 0)
            .await
            .unwrap();

        let released = store.release_expired_reservations(1_000).await;
        assert_eq!(released, 1);

        let snap = store.snapshot().await;
        assert_eq!(snap.available_utxos, 1);
        assert_eq!(snap.reserved_utxos, 1);
    }

    #[tokio::test]
    async fn mark_spent_from_reserved_then_idempotent_repeat() {
        let store = InMemoryBtcInventoryStore::with_seed(vec![available(0, 100_000)]);
        let res = store
            .reserve(&rfq(), &quote("q1"), &[op(0)], 1_000, 0)
            .await
            .unwrap();

        let n = store.mark_spent(&res, "wt-1".to_owned(), 10).await.unwrap();
        assert_eq!(n, 1);

        // Repeat with the same witness_txid is a no-op success (0 updated).
        let again = store.mark_spent(&res, "wt-1".to_owned(), 20).await.unwrap();
        assert_eq!(again, 0);

        let snap = store.snapshot().await;
        assert_eq!(snap.spent_utxos, 1);
    }

    #[tokio::test]
    async fn pending_confirm_then_spent_then_reorg_recovers_input() {
        let store = InMemoryBtcInventoryStore::with_seed(vec![available(0, 100_000)]);
        let res = store
            .reserve(&rfq(), &quote("q1"), &[op(0)], 1_000, 0)
            .await
            .unwrap();
        store
            .mark_pending_bitcoin_confirm(&res, "wt-7".to_owned(), 10)
            .await
            .unwrap();
        store.mark_spent(&res, "wt-7".to_owned(), 20).await.unwrap();

        // Reorg orphans wt-7: the spent input returns to Available.
        let affected = store.mark_reorged("wt-7", 30).await.unwrap();
        assert_eq!(affected, 1);
        assert!(matches!(
            store.get(&op(0)).await.unwrap().status,
            BtcInventoryStatus::Available
        ));
    }

    #[tokio::test]
    async fn reorg_invalidates_pending_change_output() {
        // A change UTXO ingested as PendingBitcoinConfirm becomes Invalid when
        // its producing tx is orphaned.
        let store = InMemoryBtcInventoryStore::new();
        store
            .ingest_change_utxo(utxo(
                5,
                40_000,
                BtcInventoryStatus::PendingBitcoinConfirm {
                    reservation_id: ReservationId("r".to_owned()),
                    witness_txid: "wt-9".to_owned(),
                },
            ))
            .await
            .unwrap();

        store.mark_reorged("wt-9", 10).await.unwrap();
        assert!(matches!(
            store.get(&op(5)).await.unwrap().status,
            BtcInventoryStatus::Invalid { .. }
        ));
    }

    #[tokio::test]
    async fn ingest_change_utxo_rejects_duplicate_outpoint() {
        let store = InMemoryBtcInventoryStore::with_seed(vec![available(0, 100_000)]);
        let dup = available(0, 999);
        assert!(matches!(
            store.ingest_change_utxo(dup).await,
            Err(BtcInventoryError::UtxoNotAvailable { .. })
        ));
    }

    #[tokio::test]
    async fn extend_reservation_pushes_out_deadline() {
        let store = InMemoryBtcInventoryStore::with_seed(vec![available(0, 100_000)]);
        let res = store
            .reserve(&rfq(), &quote("q1"), &[op(0)], 500, 0)
            .await
            .unwrap();

        // Extend past the original 500ms deadline.
        let moved = store.extend_reservation(&res, 10_000, 100).await.unwrap();
        assert_eq!(moved, 1);

        // The reservation now survives a sweep that would have freed it.
        assert_eq!(store.release_expired_reservations(1_000).await, 0);
        assert_eq!(store.snapshot().await.reserved_utxos, 1);
    }

    #[tokio::test]
    async fn extend_reservation_unknown_id_errors() {
        let store = InMemoryBtcInventoryStore::with_seed(vec![available(0, 100_000)]);
        assert!(matches!(
            store
                .extend_reservation(&ReservationId("nope".to_owned()), 10_000, 0)
                .await,
            Err(BtcInventoryError::ReservationNotFound(_))
        ));
    }

    #[tokio::test]
    async fn mark_broadcast_failed_releases_reservation() {
        let store = InMemoryBtcInventoryStore::with_seed(vec![available(0, 100_000)]);
        let res = store
            .reserve(&rfq(), &quote("q1"), &[op(0)], 1_000, 0)
            .await
            .unwrap();

        store.mark_broadcast_failed(&res, 10).await.unwrap();
        assert!(matches!(
            store.get(&op(0)).await.unwrap().status,
            BtcInventoryStatus::Available
        ));
    }

    #[tokio::test]
    async fn find_reservation_for_quote_round_trips() {
        let store = InMemoryBtcInventoryStore::with_seed(vec![available(0, 100_000)]);
        let res = store
            .reserve(&rfq(), &quote("q-find"), &[op(0)], 1_000, 0)
            .await
            .unwrap();

        assert_eq!(
            store.find_reservation_for_quote(&quote("q-find")).await,
            Some(res)
        );
        assert_eq!(
            store.find_reservation_for_quote(&quote("q-missing")).await,
            None
        );
    }

    #[tokio::test]
    async fn concurrent_reserves_do_not_double_book() {
        // Five UTXOs; 10 concurrent single-UTXO reservations. At most 5 win.
        let store = Arc::new(InMemoryBtcInventoryStore::with_seed(
            (0..5).map(|i| available(i, 100_000)).collect(),
        ));

        let mut handles = Vec::new();
        for i in 0..10u32 {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                store
                    .reserve(&rfq(), &quote(&format!("q{i}")), &[op(i % 5)], 1_000, 0)
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
        assert_eq!(wins, 5);
        assert_eq!(store.snapshot().await.reserved_utxos, 5);
    }

    // --- coin selection ---

    #[test]
    fn greedy_selects_single_largest_when_it_covers() {
        let sel = GreedyLargestFirstSelector;
        let utxos = vec![available(0, 30_000), available(1, 100_000), available(2, 50_000)];
        let picked = sel.select(40_000, &utxos).unwrap();
        assert_eq!(picked.chosen, vec![op(1)]);
        assert_eq!(picked.total_chosen_sats, 100_000);
        assert_eq!(picked.change_sats(), 60_000);
    }

    #[test]
    fn greedy_accumulates_multiple_when_needed() {
        let sel = GreedyLargestFirstSelector;
        let utxos = vec![available(0, 30_000), available(1, 50_000), available(2, 40_000)];
        // Need 80k: picks 50k then 40k = 90k.
        let picked = sel.select(80_000, &utxos).unwrap();
        assert_eq!(picked.chosen, vec![op(1), op(2)]);
        assert_eq!(picked.total_chosen_sats, 90_000);
    }

    #[test]
    fn greedy_errors_when_total_is_insufficient() {
        let sel = GreedyLargestFirstSelector;
        let utxos = vec![available(0, 10_000), available(1, 20_000)];
        assert_eq!(
            sel.select(100_000, &utxos),
            Err(BtcCoinSelectionError::Insufficient {
                requested: 100_000,
                available: 30_000,
            })
        );
    }

    #[test]
    fn greedy_exact_match_produces_zero_change() {
        let sel = GreedyLargestFirstSelector;
        let utxos = vec![available(0, 75_000)];
        let picked = sel.select(75_000, &utxos).unwrap();
        assert_eq!(picked.change_sats(), 0);
    }
}
