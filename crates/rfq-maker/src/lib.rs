use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use rfq_btc::BitcoinClient;
use rfq_rgb::{RgbBackend, TxOut};
use rfq_router::{MakerConnector, RouterError};
use rfq_store::{
    BtcCoinSelector, BtcInventoryStore, GreedyLargestFirstSelector, InMemoryBtcInventoryStore,
    InMemoryInventoryStore, InventoryStore,
};
use rfq_types::{
    AcceptQuoteRequest, AssetId, BtcInventoryStatus, BtcInventoryUtxo, ExtendedInventorySnapshot,
    InventoryError, InventorySnapshot, InventoryStatus, InventoryUtxo, MakerId, Outpoint, Quote,
    QuoteId, QuoteRequest, RgbInventoryUtxo, ReservationId, SettlementIntent, SettlementStatus,
    Side, SwapLeg,
};
use tokio::sync::RwLock;
use uuid::Uuid;

mod coin_select;
pub use coin_select::{CoinSelectionError, CoinSelector, GreedyExactFitSelector, Selection};

/// Inputs to the periodic rebalance planner. Defaults are conservative — the
/// loop should fire rarely under normal operation. See
/// `docs/rebalancing-strategy.md` for the why behind these thresholds.
#[derive(Debug, Clone)]
pub struct RebalancePolicy {
    pub fragmentation_threshold: f64,
    pub max_utxo_count: u64,
    pub min_utxo_count: u64,
}

impl Default for RebalancePolicy {
    fn default() -> Self {
        Self {
            fragmentation_threshold: 0.7,
            max_utxo_count: 50,
            min_utxo_count: 3,
        }
    }
}

/// Proposed rebalance actions plus the trigger reasons that fired. In 14e
/// `merges` and `splits` are always empty placeholders — execution is deferred
/// to the follow-up issue. The trigger list is what the loop logs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RebalancePlan {
    pub triggers: Vec<RebalanceTrigger>,
    pub merges: Vec<MergeAction>,
    pub splits: Vec<SplitAction>,
}

impl RebalancePlan {
    pub fn is_empty(&self) -> bool {
        self.triggers.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RebalanceTrigger {
    HighFragmentation { score: f64, threshold: f64 },
    TooManyUtxos { count: u64, max: u64 },
    TooFewUtxos { count: u64, min: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MergeAction {
    pub asset: AssetId,
    pub inputs: Vec<Outpoint>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SplitAction {
    pub asset: AssetId,
    pub input: Outpoint,
    pub target_pieces: u32,
}

const QUOTE_TTL_MS: u64 = 30_000;
/// After the taker accepts, the maker holds the reservation this long awaiting
/// the signed PSBT via `/sign` — longer than `QUOTE_TTL_MS` because the taker
/// now has to add BTC inputs and sign. See `docs/swap-flows.md`.
const TAKER_SIGNATURE_TTL_MS: u64 = 600_000;
/// After broadcast, the reservation sits in `PendingBitcoinConfirm` until the
/// witness tx confirms (or a reorg / timeout intervenes).
const BROADCAST_CONFIRM_TTL_MS: u64 = 7_200_000;
/// Rough vbyte footprint of a 2-in / 3-out segwit swap tx. Multiplied by the
/// feerate estimate to turn it into an absolute fee on the quote. A real
/// estimate would size the actual PSBT; this is a deliberate v0 placeholder.
const ESTIMATED_SWAP_VBYTES: u64 = 200;
/// Upper bound on reservation retries under contention. With outpoint
/// exclusion on each retry, the effective bound is `min(this, available_utxo_count)`
/// — the loop exits via the selector's Insufficient branch once exclusions
/// have whittled the candidate set to empty.
const RESERVE_RETRY_ATTEMPTS: u32 = 16;
/// Sell side: after the taker accepts, the maker holds its BTC reservation
/// this long awaiting the taker's consignment via `/consignment`. Building an
/// RGB consignment is a wallet operation, so the window is generous.
const CONSIGNMENT_TTL_MS: u64 = 600_000;

/// Maker-side state for a buy-side swap held between `accept_quote` and
/// `submit_signed_psbt`. The settlement state machine (#9) will formalize
/// this; for the mock a per-quote map keyed by `QuoteId` is enough.
#[derive(Clone)]
struct PendingBuySettlement {
    quote: Quote,
    reservation_id: ReservationId,
    /// Maker-built consignment handed to the taker at accept; needed again to
    /// finalize once the taker returns the signed PSBT.
    consignment: String,
    /// Sats the maker over-selected — re-ingested as a change UTXO post-broadcast.
    expected_change: u64,
}

/// Maker-side state for a sell-side swap. Created at `accept_quote` (stage
/// `AwaitingConsignment`, `psbt_built` is `None`) and advanced by
/// `deliver_consignment`, which fills `psbt_built` once the taker's
/// consignment has been validated and the swap PSBT built.
#[derive(Clone)]
struct PendingSellSettlement {
    quote: Quote,
    btc_reservation_id: ReservationId,
    btc_payout_addr: String,
    rgb_change_invoice: Option<String>,
    psbt_built: Option<SellPsbtBuilt>,
}

/// The `deliver_consignment` half of a sell-side settlement.
#[derive(Clone)]
struct SellPsbtBuilt {
    /// The taker's RGB-bearing outpoints from the validated consignment. The
    /// bait-and-switch check at `/sign` verifies the signed PSBT still spends
    /// every one of them.
    consigned_outpoints: Vec<Outpoint>,
    /// Witness txid committed at PSBT-build time; the signed PSBT must hash to it.
    expected_witness_txid: String,
    /// The taker's consignment, replayed into `finalize_after_taker_sign`.
    consignment: String,
}

#[derive(Clone)]
enum PendingSettlement {
    Buy(PendingBuySettlement),
    Sell(PendingSellSettlement),
}

#[derive(Clone)]
pub struct Maker {
    maker_id: MakerId,
    store: Arc<dyn InventoryStore>,
    selector: Arc<dyn CoinSelector>,
    /// Plain-BTC inventory the maker pays out from on the sell side. Empty
    /// unless seeded via `with_btc_inventory`; a buy-only maker never touches it.
    btc_store: Arc<dyn BtcInventoryStore>,
    rgb_backend: Arc<dyn RgbBackend>,
    bitcoin_client: Arc<dyn BitcoinClient>,
    pending: Arc<RwLock<HashMap<QuoteId, PendingSettlement>>>,
}

impl Maker {
    /// Seed the inventory store from a per-UTXO chain view. All entries start
    /// `Available`; tests needing pre-existing Reserved / Spent state should
    /// build their own `InMemoryInventoryStore` and use `with_components`.
    pub fn new(
        maker_id: MakerId,
        utxos: Vec<RgbInventoryUtxo>,
        rgb_backend: Arc<dyn RgbBackend>,
        bitcoin_client: Arc<dyn BitcoinClient>,
    ) -> Self {
        let now = now_ms();
        let inv_utxos: Vec<InventoryUtxo> = utxos
            .into_iter()
            .map(|u| InventoryUtxo {
                outpoint: u.outpoint,
                asset_id: u.asset_id,
                amount: u.amount,
                btc_sats: u.btc_sats,
                status: InventoryStatus::Available,
                created_at_ms: now,
                updated_at_ms: now,
                pending_txid: None,
            })
            .collect();
        Self::with_components(
            maker_id,
            Arc::new(InMemoryInventoryStore::with_seed(inv_utxos)),
            Arc::new(GreedyExactFitSelector),
            rgb_backend,
            bitcoin_client,
        )
    }

    /// Full-control constructor: caller supplies its own inventory store and
    /// coin selector. Forward path for #9 (settlement state machine) once the
    /// store needs to be shared across components. The BTC inventory starts
    /// empty — seed it for sell-side quoting with [`Maker::with_btc_inventory`].
    pub fn with_components(
        maker_id: MakerId,
        store: Arc<dyn InventoryStore>,
        selector: Arc<dyn CoinSelector>,
        rgb_backend: Arc<dyn RgbBackend>,
        bitcoin_client: Arc<dyn BitcoinClient>,
    ) -> Self {
        Self {
            maker_id,
            store,
            selector,
            btc_store: Arc::new(InMemoryBtcInventoryStore::new()),
            rgb_backend,
            bitcoin_client,
            pending: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Seed the maker's plain-BTC inventory — the pool it pays sell-side takers
    /// from. Without this a maker quotes buy-side only; a sell `request_quote`
    /// finds no BTC to reserve and returns no quote.
    pub fn with_btc_inventory(mut self, utxos: Vec<BtcInventoryUtxo>) -> Self {
        self.btc_store = Arc::new(InMemoryBtcInventoryStore::with_seed(utxos));
        self
    }

    /// Per-UTXO view across all assets. Returned in outpoint order so callers
    /// can index deterministically.
    pub async fn utxo_snapshot(&self) -> Vec<InventoryUtxo> {
        let mut utxos = self.store.list_all().await;
        utxos.sort_by(|a, b| a.outpoint.cmp(&b.outpoint));
        utxos
    }

    pub async fn inventory_summary(&self) -> InventorySnapshot {
        let ext = self.extended_inventory_summary().await;
        (&ext).into()
    }

    /// Forward-looking metric surface used by 14e's rebalance loop. The legacy
    /// `inventory_summary` keeps wire-format stability for the HTTP layer.
    pub async fn extended_inventory_summary(&self) -> ExtendedInventorySnapshot {
        self.store.release_expired_reservations(now_ms()).await;
        let utxos = self.store.list_all().await;
        ExtendedInventorySnapshot::from_utxos(utxos.iter())
    }

    pub async fn release_expired_reservations(&self) -> usize {
        let now = now_ms();
        // Both pools age out here, so the maker-node cleanup loop's single call
        // covers buy-side RGB reservations and sell-side BTC reservations alike.
        let released = self.store.release_expired_reservations(now).await
            + self.btc_store.release_expired_reservations(now).await;
        // Prune settlement state whose reservation no longer exists — either
        // expired above, or already transitioned out of `Reserved` by a
        // completed settlement. `find_reservation_for_quote` only matches
        // `Reserved`; a settled quote reads as "gone", which is fine — the
        // settlement path already removed its own entry. Buy and sell track
        // their reservation in different stores.
        let entries: Vec<(QuoteId, bool)> = self
            .pending
            .read()
            .await
            .iter()
            .map(|(quote_id, p)| (quote_id.clone(), matches!(p, PendingSettlement::Buy(_))))
            .collect();
        let mut stale = Vec::new();
        for (quote_id, is_buy) in entries {
            let live = if is_buy {
                self.store
                    .find_reservation_for_quote(&quote_id)
                    .await
                    .is_some()
            } else {
                self.btc_store
                    .find_reservation_for_quote(&quote_id)
                    .await
                    .is_some()
            };
            if !live {
                stale.push(quote_id);
            }
        }
        if !stale.is_empty() {
            let mut pending = self.pending.write().await;
            for quote_id in stale {
                pending.remove(&quote_id);
            }
        }
        released
    }

    /// Periodic rebalance planner. Reads `extended_inventory_summary()` and
    /// returns a `RebalancePlan` describing trigger reasons plus (in the
    /// follow-up issue) the merge/split actions to fold into the next
    /// outgoing settlement tx. In 14e the action lists stay empty — only the
    /// trigger detection ships. See `docs/rebalancing-strategy.md`.
    pub async fn rebalance(&self, policy: &RebalancePolicy) -> RebalancePlan {
        let ext = self.extended_inventory_summary().await;
        let mut plan = RebalancePlan::default();

        // High fragmentation is only meaningful when there's something to
        // rebalance — guard against div-by-zero artifacts on empty inventory.
        if ext.available_amount > 0 && ext.fragmentation_score >= policy.fragmentation_threshold {
            plan.triggers.push(RebalanceTrigger::HighFragmentation {
                score: ext.fragmentation_score,
                threshold: policy.fragmentation_threshold,
            });
        }
        if ext.available_utxos > policy.max_utxo_count {
            plan.triggers.push(RebalanceTrigger::TooManyUtxos {
                count: ext.available_utxos,
                max: policy.max_utxo_count,
            });
        }
        if ext.available_utxos < policy.min_utxo_count && ext.total_amount > 0 {
            plan.triggers.push(RebalanceTrigger::TooFewUtxos {
                count: ext.available_utxos,
                min: policy.min_utxo_count,
            });
        }

        plan
    }

    /// Sell-side `request_quote`: the maker reserves BTC to pay the taker and
    /// mints the RGB invoice the taker will consign to. Returns `Ok(None)`
    /// when the BTC pool can't cover the gross payout.
    async fn request_quote_sell(
        &self,
        request: QuoteRequest,
        quote_id: QuoteId,
        expires_at_ms: u64,
    ) -> Result<Option<Quote>, RouterError> {
        let amount = request.amount;
        // Same pricing rule as the buy side; `price` is the gross BTC the
        // maker pays out, before the network fee the taker covers.
        let gross_btc_sats = amount.saturating_mul(101);

        let available = self.btc_store.list_available().await;
        let selection = match GreedyLargestFirstSelector.select(gross_btc_sats, &available) {
            Ok(s) => s,
            // Not enough BTC on hand — decline the quote rather than error.
            Err(_) => return Ok(None),
        };
        let reservation_id = match self
            .btc_store
            .reserve(
                &request.rfq_id,
                &quote_id,
                &selection.chosen,
                expires_at_ms,
                now_ms(),
            )
            .await
        {
            Ok(rid) => rid,
            // Lost a race for one of the chosen UTXOs — treat as no quote.
            Err(_) => return Ok(None),
        };

        // Mint the maker's RGB invoice. On failure, hand the BTC back.
        let maker_rgb_invoice = match self
            .rgb_backend
            .create_invoice(&request.base_asset, amount)
            .await
        {
            Ok(inv) => inv,
            Err(e) => {
                let _ = self
                    .btc_store
                    .release_reservation(&reservation_id, now_ms())
                    .await;
                return Err(RouterError::Maker(e.to_string()));
            }
        };

        Ok(Some(Quote {
            quote_id,
            rfq_id: request.rfq_id,
            maker_id: self.maker_id.clone(),
            base_asset: request.base_asset,
            quote_asset: request.quote_asset,
            side: request.side,
            amount,
            price: gross_btc_sats,
            expires_at_ms,
            estimated_fee_sats: self.estimate_swap_fee().await,
            fee_slippage_bps: 2000,
            maker_rgb_invoice: Some(maker_rgb_invoice),
        }))
    }

    /// Quote-time fee estimate: feerate × the rough swap-tx vbyte footprint. A
    /// failed estimate falls back to 0, which disables the slippage check at
    /// settlement (no baseline to compare against) rather than rejecting the
    /// quote outright.
    async fn estimate_swap_fee(&self) -> u64 {
        let feerate = self.bitcoin_client.estimate_feerate(3).await.unwrap_or(0);
        feerate.saturating_mul(ESTIMATED_SWAP_VBYTES)
    }

    /// Sell-side `accept_quote`: validate the payout address, re-confirm the
    /// BTC reservation made at quote time, stretch it to the consignment
    /// window, and park a `PendingSellSettlement` in `AwaitingConsignment`.
    async fn accept_quote_sell(
        &self,
        quote: Quote,
        btc_payout_addr: String,
        rgb_change_invoice: Option<String>,
    ) -> Result<SettlementIntent, RouterError> {
        self.btc_store.release_expired_reservations(now_ms()).await;

        // Mock address check — real validation parses the bech32 / base58 form.
        if btc_payout_addr.trim().is_empty() {
            return Err(RouterError::Maker(
                "sell leg is missing a BTC payout address".to_owned(),
            ));
        }

        let btc_reservation_id = self
            .btc_store
            .find_reservation_for_quote(&quote.quote_id)
            .await
            .ok_or_else(|| {
                RouterError::Maker("quote BTC reservation not found or expired".to_owned())
            })?;

        // The quote-stage TTL is short; stretch it to the consignment window
        // so the cleanup loop doesn't reclaim the BTC mid-flight.
        let expires_at_ms = now_ms() + CONSIGNMENT_TTL_MS;
        self.btc_store
            .extend_reservation(&btc_reservation_id, expires_at_ms, now_ms())
            .await
            .map_err(|e| RouterError::Maker(e.to_string()))?;

        let quote_id = quote.quote_id.clone();
        self.pending.write().await.insert(
            quote_id.clone(),
            PendingSettlement::Sell(PendingSellSettlement {
                quote,
                btc_reservation_id,
                btc_payout_addr,
                rgb_change_invoice,
                psbt_built: None,
            }),
        );

        Ok(SettlementIntent {
            quote_id,
            maker_id: self.maker_id.clone(),
            status: SettlementStatus::AwaitingConsignment,
            transfer: None,
            expires_at_ms,
            witness_txid: None,
            final_consignment: None,
        })
    }

    /// Sell-side `/consignment`: validate the taker's consignment, resolve the
    /// prevouts it names, re-check fee slippage, build + maker-sign the swap
    /// PSBT, and advance the settlement to `AwaitingTakerSignature`.
    async fn deliver_consignment_sell(
        &self,
        quote_id: QuoteId,
        consignment_base64: String,
    ) -> Result<SettlementIntent, RouterError> {
        self.btc_store.release_expired_reservations(now_ms()).await;

        let pending = match self.pending.read().await.get(&quote_id).cloned() {
            Some(PendingSettlement::Sell(p)) => p,
            // A buy-side entry, or no accepted sell quote at all.
            _ => {
                return Err(RouterError::Maker(
                    "no accepted sell-side settlement for quote".to_owned(),
                ))
            }
        };

        // A vanished reservation means the consignment window lapsed.
        let btc_reservation_id = match self
            .btc_store
            .find_reservation_for_quote(&quote_id)
            .await
        {
            Some(rid) => rid,
            None => {
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::Maker(
                    "consignment window lapsed before delivery".to_owned(),
                ));
            }
        };

        let quote = &pending.quote;
        let maker_rgb_invoice = quote.maker_rgb_invoice.clone().ok_or_else(|| {
            RouterError::Maker("sell quote is missing its maker RGB invoice".to_owned())
        })?;

        // Round-trip the maker's own invoice back into typed components for
        // `create_swap_psbt_sell` below. Done once here so the backend doesn't
        // re-parse the wire string it just minted.
        let maker_invoice_parts = self
            .rgb_backend
            .parse_maker_invoice(&maker_rgb_invoice)
            .await
            .map_err(|e| RouterError::Maker(format!("maker invoice parse: {e}")))?;

        // Validate the consignment against the contract the maker quoted.
        // Uses the typed `contract_id` from `parse_maker_invoice` above to
        // avoid the self-round-trip of re-parsing the maker's own invoice
        // string inside the backend.
        let info = match self
            .rgb_backend
            .validate_incoming_consignment(
                &consignment_base64,
                maker_invoice_parts.contract_id,
            )
            .await
        {
            Ok(info) => info,
            Err(e) => {
                let _ = self
                    .btc_store
                    .mark_broadcast_failed(&btc_reservation_id, now_ms())
                    .await;
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::ConsignmentRejected(e.to_string()));
            }
        };

        // Resolve the prevout behind every consigned RGB outpoint — needed to
        // pin them as PSBT inputs. An outpoint the chain doesn't know (or a
        // non-segwit one) makes the consignment unusable.
        let mut taker_rgb_prevouts = Vec::with_capacity(info.outpoints.len());
        for outpoint in &info.outpoints {
            match self.bitcoin_client.get_outpoint(outpoint).await {
                Ok(txout) => taker_rgb_prevouts.push((outpoint.clone(), txout)),
                Err(e) => {
                    let _ = self
                        .btc_store
                        .mark_broadcast_failed(&btc_reservation_id, now_ms())
                        .await;
                    self.pending.write().await.remove(&quote_id);
                    return Err(RouterError::ConsignmentRejected(format!(
                        "consignment names an unusable outpoint {outpoint}: {e}"
                    )));
                }
            }
        }

        // The maker's own BTC inputs are the UTXOs reserved under this quote;
        // their value + script live in the BTC store, so no chain round trip.
        let maker_btc_inputs: Vec<(Outpoint, TxOut)> = self
            .btc_store
            .list_all()
            .await
            .into_iter()
            .filter(|u| {
                matches!(&u.status,
                    BtcInventoryStatus::Reserved { reservation_id: rid, .. }
                        if rid == &btc_reservation_id)
            })
            .map(|u| {
                (
                    u.outpoint,
                    TxOut {
                        value_sats: u.value_sats,
                        script_pubkey: u.script_pubkey,
                    },
                )
            })
            .collect();

        // Re-estimate the fee and abort if it blew the quote's slippage cap.
        let actual_fee = self.estimate_swap_fee().await;
        if quote.estimated_fee_sats > 0 {
            let cap = quote
                .estimated_fee_sats
                .saturating_mul(10_000 + u64::from(quote.fee_slippage_bps))
                / 10_000;
            if actual_fee > cap {
                let _ = self
                    .btc_store
                    .mark_broadcast_failed(&btc_reservation_id, now_ms())
                    .await;
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::FeeSlippageExceeded {
                    estimated: quote.estimated_fee_sats,
                    actual: actual_fee,
                });
            }
        }

        // Build the swap PSBT (maker BTC inputs signed in the mock string).
        // `deliver_amount` falls back to the consigned total when the maker's
        // invoice was minted amountless (current `create_invoice` always pins
        // an amount, but the fallback keeps us future-proof).
        let deliver_amount = maker_invoice_parts.amount.unwrap_or(info.total_amount);
        let transfer = match self
            .rgb_backend
            .create_swap_psbt_sell(
                &info,
                &taker_rgb_prevouts,
                &maker_btc_inputs,
                maker_invoice_parts.contract_id,
                maker_invoice_parts.seal,
                deliver_amount,
                &pending.btc_payout_addr,
                pending.rgb_change_invoice.as_deref(),
                quote.price,
                actual_fee,
            )
            .await
        {
            Ok(t) => t,
            Err(e) => {
                let _ = self
                    .btc_store
                    .mark_broadcast_failed(&btc_reservation_id, now_ms())
                    .await;
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::Maker(e.to_string()));
            }
        };

        let expected_witness_txid = transfer.expected_witness_txid.clone().ok_or_else(|| {
            RouterError::Maker("sell PSBT carries no committed witness txid".to_owned())
        })?;

        // Stretch the reservation to the taker-signature window.
        let expires_at_ms = now_ms() + TAKER_SIGNATURE_TTL_MS;
        self.btc_store
            .extend_reservation(&btc_reservation_id, expires_at_ms, now_ms())
            .await
            .map_err(|e| RouterError::Maker(e.to_string()))?;

        // Record what `/sign` will re-check the taker's PSBT against.
        if let Some(PendingSettlement::Sell(p)) = self.pending.write().await.get_mut(&quote_id) {
            p.psbt_built = Some(SellPsbtBuilt {
                consigned_outpoints: info.outpoints,
                expected_witness_txid,
                consignment: consignment_base64,
            });
        }

        Ok(SettlementIntent {
            quote_id,
            maker_id: self.maker_id.clone(),
            status: SettlementStatus::AwaitingTakerSignature,
            transfer: Some(transfer),
            expires_at_ms,
            witness_txid: None,
            final_consignment: None,
        })
    }

    /// Sell-side `/sign`: guard against input swaps and txid drift, then
    /// finalize, broadcast, and move the maker's BTC to `PendingBitcoinConfirm`.
    async fn submit_signed_psbt_sell(
        &self,
        quote_id: QuoteId,
        pending: PendingSellSettlement,
        signed_psbt_base64: String,
    ) -> Result<SettlementIntent, RouterError> {
        let built = pending.psbt_built.as_ref().ok_or_else(|| {
            RouterError::Maker("consignment not yet delivered for this quote".to_owned())
        })?;
        let btc_reservation_id = pending.btc_reservation_id.clone();

        // A vanished reservation means the taker-signature window lapsed.
        if self
            .btc_store
            .find_reservation_for_quote(&quote_id)
            .await
            .is_none()
        {
            self.pending.write().await.remove(&quote_id);
            return Err(RouterError::Maker(
                "settlement expired before signature was submitted".to_owned(),
            ));
        }

        // Bait-and-switch guard: the signed PSBT must still spend every
        // outpoint the validated consignment named.
        for outpoint in &built.consigned_outpoints {
            if !signed_psbt_base64.contains(&outpoint.to_string()) {
                let _ = self
                    .btc_store
                    .mark_broadcast_failed(&btc_reservation_id, now_ms())
                    .await;
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::PsbtInvalid(
                    "input set diverged from consignment".to_owned(),
                ));
            }
        }

        // The witness txid was committed at PSBT-build time; a signed PSBT
        // that no longer carries it has been tampered with.
        if !signed_psbt_base64.contains(&built.expected_witness_txid) {
            let _ = self
                .btc_store
                .mark_broadcast_failed(&btc_reservation_id, now_ms())
                .await;
            self.pending.write().await.remove(&quote_id);
            return Err(RouterError::PsbtInvalid("txid mismatch".to_owned()));
        }

        let finalized = match self
            .rgb_backend
            .finalize_after_taker_sign(&signed_psbt_base64, &built.consignment)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                let _ = self
                    .btc_store
                    .mark_broadcast_failed(&btc_reservation_id, now_ms())
                    .await;
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::PsbtInvalid(e.to_string()));
            }
        };

        // Broadcast. A failure here releases the BTC — the swap tx never hit
        // the network, so the maker's UTXOs are still spendable.
        if let Err(e) = self.bitcoin_client.broadcast(&finalized.raw_tx).await {
            let _ = self
                .btc_store
                .mark_broadcast_failed(&btc_reservation_id, now_ms())
                .await;
            self.pending.write().await.remove(&quote_id);
            return Err(RouterError::Maker(format!("broadcast failed: {e}")));
        }

        self.btc_store
            .mark_pending_bitcoin_confirm(&btc_reservation_id, finalized.witness_txid.clone(), now_ms())
            .await
            .map_err(|e| RouterError::Maker(e.to_string()))?;

        self.pending.write().await.remove(&quote_id);

        Ok(SettlementIntent {
            quote_id,
            maker_id: self.maker_id.clone(),
            status: SettlementStatus::PendingBitcoinConfirm,
            transfer: None,
            expires_at_ms: now_ms() + BROADCAST_CONFIRM_TTL_MS,
            witness_txid: Some(finalized.witness_txid),
            final_consignment: Some(finalized.final_consignment_base64),
        })
    }
}

#[async_trait]
impl MakerConnector for Maker {
    fn maker_id(&self) -> MakerId {
        self.maker_id.clone()
    }

    async fn request_quote(&self, request: QuoteRequest) -> Result<Option<Quote>, RouterError> {
        let now = now_ms();
        self.store.release_expired_reservations(now).await;
        self.btc_store.release_expired_reservations(now).await;

        let quote_id = QuoteId(Uuid::new_v4().to_string());
        let expires_at_ms = now + QUOTE_TTL_MS;

        // Sell side reserves BTC, not RGB — the maker is the one paying out.
        if matches!(request.side, Side::Sell) {
            return self.request_quote_sell(request, quote_id, expires_at_ms).await;
        }

        // Exclusion-based retry: when a reservation fails because another
        // caller grabbed an outpoint between our list_available read and our
        // reserve_utxos write, we exclude the contested outpoint and re-select.
        // This is what gives a deterministic selector (GreedyExactFitSelector)
        // healthy concurrency — without it, every losing task re-picks the
        // same UTXO and only one ever makes progress per round.
        let mut excluded: HashSet<Outpoint> = HashSet::new();
        let mut attempts: u32 = 0;
        let selection = loop {
            attempts += 1;
            let available: Vec<InventoryUtxo> = self
                .store
                .list_available(&request.base_asset)
                .await
                .into_iter()
                .filter(|u| !excluded.contains(&u.outpoint))
                .collect();
            let selection = match self
                .selector
                .select(&request.base_asset, request.amount, &available)
            {
                Ok(s) => s,
                Err(CoinSelectionError::Insufficient { .. }) => return Ok(None),
                Err(e) => return Err(RouterError::Maker(e.to_string())),
            };
            match self
                .store
                .reserve_utxos(
                    &request.rfq_id,
                    &quote_id,
                    &selection.chosen,
                    expires_at_ms,
                    now_ms(),
                )
                .await
            {
                Ok(_) => break selection,
                Err(InventoryError::UtxoNotAvailable { .. })
                | Err(InventoryError::UtxoNotFound(_))
                    if attempts < RESERVE_RETRY_ATTEMPTS =>
                {
                    excluded.extend(selection.chosen);
                    continue;
                }
                Err(InventoryError::UtxoNotAvailable { .. })
                | Err(InventoryError::UtxoNotFound(_)) => return Ok(None),
                Err(e) => return Err(RouterError::Maker(e.to_string())),
            }
        };

        let estimated_fee_sats = self.estimate_swap_fee().await;

        Ok(Some(Quote {
            quote_id,
            rfq_id: request.rfq_id,
            maker_id: self.maker_id.clone(),
            base_asset: request.base_asset,
            quote_asset: request.quote_asset,
            side: request.side,
            amount: selection.requested,
            price: selection.requested.saturating_mul(101),
            expires_at_ms,
            estimated_fee_sats,
            // 20% slippage cap — the v0 default per docs/swap-flows.md.
            fee_slippage_bps: 2000,
            // Buy-side quotes never carry a maker RGB invoice — the taker
            // supplies its own at `/accept`. Sell side sets it (see
            // `request_quote_sell`).
            maker_rgb_invoice: None,
        }))
    }

    async fn accept_quote(
        &self,
        quote: Quote,
        request: AcceptQuoteRequest,
    ) -> Result<SettlementIntent, RouterError> {
        // Buy and sell diverge entirely at accept: buy finalizes the maker's
        // RGB-side PSBT now, sell only parks the BTC reservation and waits for
        // the taker's consignment. Dispatch and let each path run on its own.
        let (rgb_invoice, btc_funding_addr) = match &request.leg {
            SwapLeg::Buy {
                rgb_invoice,
                btc_funding_addr,
            } => (rgb_invoice.clone(), btc_funding_addr.clone()),
            SwapLeg::Sell {
                btc_payout_addr,
                rgb_change_invoice,
            } => {
                return self
                    .accept_quote_sell(
                        quote,
                        btc_payout_addr.clone(),
                        rgb_change_invoice.clone(),
                    )
                    .await;
            }
        };

        let now = now_ms();
        self.store.release_expired_reservations(now).await;

        let reservation_id = self
            .store
            .find_reservation_for_quote(&quote.quote_id)
            .await
            .ok_or_else(|| {
                RouterError::Maker("quote reservation not found or expired".to_owned())
            })?;

        // Reserved UTXOs for this quote: their outpoints feed the swap PSBT,
        // and their summed amount minus the quote amount is the maker's RGB
        // change. `Reserved` is the only status still carrying reservation_id.
        let reserved: Vec<InventoryUtxo> = self
            .store
            .list_for_asset(&quote.base_asset)
            .await
            .into_iter()
            .filter(|u| {
                matches!(&u.status,
                    InventoryStatus::Reserved { reservation_id: rid, .. } if rid == &reservation_id)
            })
            .collect();
        let reserved_total: u64 = reserved.iter().map(|u| u.amount).sum();
        let reserved_outpoints: Vec<Outpoint> =
            reserved.iter().map(|u| u.outpoint.clone()).collect();
        let expected_change = reserved_total.saturating_sub(quote.amount);

        // Declared-funding: discover the taker's BTC UTXOs from the address it
        // put on the ACCEPT, then select enough to cover the price + fee. The
        // taker only signs these inputs at `/sign`; it never adds its own.
        let actual_fee = self.estimate_swap_fee().await;
        if quote.estimated_fee_sats > 0 {
            let cap = quote
                .estimated_fee_sats
                .saturating_mul(10_000 + u64::from(quote.fee_slippage_bps))
                / 10_000;
            if actual_fee > cap {
                let _ = self.store.release_reservation(&reservation_id, now_ms()).await;
                return Err(RouterError::FeeSlippageExceeded {
                    estimated: quote.estimated_fee_sats,
                    actual: actual_fee,
                });
            }
        }
        let taker_btc_inputs = match self.bitcoin_client.list_unspent(&btc_funding_addr).await {
            Ok(utxos) => match select_btc_inputs(&utxos, quote.price.saturating_add(actual_fee)) {
                Some(selected) => selected,
                None => {
                    let _ = self.store.release_reservation(&reservation_id, now_ms()).await;
                    return Err(RouterError::Maker(format!(
                        "taker funding address {btc_funding_addr} has insufficient BTC for \
                         price {} + fee {actual_fee}",
                        quote.price,
                    )));
                }
            },
            Err(e) => {
                let _ = self.store.release_reservation(&reservation_id, now_ms()).await;
                return Err(RouterError::Maker(format!(
                    "list_unspent({btc_funding_addr}) failed: {e}"
                )));
            }
        };

        // Build the maker-RGB-side PSBT + consignment. On failure the
        // reservation goes back to Available.
        let transfer = match self
            .rgb_backend
            .create_swap_psbt_buy(
                &rgb_invoice,
                quote.amount,
                &reserved_outpoints,
                &taker_btc_inputs,
                &btc_funding_addr,
                quote.price,
                actual_fee,
            )
            .await
        {
            Ok(t) => t,
            Err(e) => {
                let _ = self
                    .store
                    .release_reservation(&reservation_id, now_ms())
                    .await;
                return Err(RouterError::Maker(e.to_string()));
            }
        };

        // The reservation carries a 30s quote TTL; extend it to the settlement
        // window so the cleanup loop doesn't release it mid-signature.
        let expires_at_ms = now_ms() + TAKER_SIGNATURE_TTL_MS;
        self.store
            .extend_reservation(&reservation_id, expires_at_ms, now_ms())
            .await
            .map_err(|e| RouterError::Maker(e.to_string()))?;

        // Stash the per-quote state `submit_signed_psbt` needs. A buy-side
        // transfer always carries `Some(consignment)`; default defensively.
        self.pending.write().await.insert(
            quote.quote_id.clone(),
            PendingSettlement::Buy(PendingBuySettlement {
                quote: quote.clone(),
                reservation_id: reservation_id.clone(),
                consignment: transfer.consignment.clone().unwrap_or_default(),
                expected_change,
            }),
        );

        Ok(SettlementIntent {
            quote_id: quote.quote_id,
            maker_id: self.maker_id.clone(),
            status: SettlementStatus::AwaitingTakerSignature,
            transfer: Some(transfer),
            expires_at_ms,
            witness_txid: None,
            final_consignment: None,
        })
    }

    async fn submit_signed_psbt(
        &self,
        quote_id: QuoteId,
        signed_psbt_base64: String,
    ) -> Result<SettlementIntent, RouterError> {
        self.store.release_expired_reservations(now_ms()).await;
        self.btc_store.release_expired_reservations(now_ms()).await;

        let pending = self
            .pending
            .read()
            .await
            .get(&quote_id)
            .cloned()
            .ok_or_else(|| RouterError::Maker("no pending settlement for quote".to_owned()))?;

        // `/sign` is shared across both swap directions — hand a sell-side
        // settlement to its own path; the rest of this method is buy-side.
        let pending = match pending {
            PendingSettlement::Buy(p) => p,
            PendingSettlement::Sell(p) => {
                return self
                    .submit_signed_psbt_sell(quote_id, p, signed_psbt_base64)
                    .await;
            }
        };

        // The reservation must still be live. If the cleanup loop already
        // released it (taker took longer than TAKER_SIGNATURE_TTL_MS), the
        // settlement is dead — drop the stale entry and report it.
        if self
            .store
            .find_reservation_for_quote(&quote_id)
            .await
            .is_none()
        {
            self.pending.write().await.remove(&quote_id);
            return Err(RouterError::Maker(
                "settlement expired before signature was submitted".to_owned(),
            ));
        }

        let reservation_id = pending.reservation_id.clone();
        let quote = &pending.quote;

        // No fee-slippage guard here: under declared-funding the buy PSBT is
        // fully committed at accept time (every input is known then), so the
        // fee was locked and slippage-checked in `accept_quote`. The taker only
        // adds signatures at `/sign` — it can't change the fee.

        // Finalize the taker-signed PSBT into a broadcastable tx.
        let finalized = match self
            .rgb_backend
            .finalize_after_taker_sign(&signed_psbt_base64, &pending.consignment)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                let _ = self
                    .store
                    .mark_broadcast_failed(&reservation_id, now_ms())
                    .await;
                self.pending.write().await.remove(&quote_id);
                return Err(RouterError::PsbtInvalid(e.to_string()));
            }
        };

        // Broadcast. A failure here releases the reservation — the swap tx
        // never hit the network, so the maker's UTXOs are still spendable.
        if let Err(e) = self.bitcoin_client.broadcast(&finalized.raw_tx).await {
            let _ = self
                .store
                .mark_broadcast_failed(&reservation_id, now_ms())
                .await;
            self.pending.write().await.remove(&quote_id);
            return Err(RouterError::Maker(format!("broadcast failed: {e}")));
        }

        // Tx is on the wire — move the reserved UTXOs to PendingBitcoinConfirm.
        self.store
            .mark_pending_bitcoin_confirm(
                &reservation_id,
                finalized.witness_txid.clone(),
                now_ms(),
            )
            .await
            .map_err(|e| RouterError::Maker(e.to_string()))?;

        // Change re-ingestion: the broadcast tx produces an RGB change output
        // back to the maker when the selection over-shot. Best-effort — an
        // ingestion collision doesn't undo a settled swap.
        if pending.expected_change > 0 {
            let now = now_ms();
            let change_utxo = InventoryUtxo {
                outpoint: Outpoint::new(finalized.witness_txid.clone(), 1),
                asset_id: quote.base_asset.clone(),
                amount: pending.expected_change,
                btc_sats: 0,
                status: InventoryStatus::PendingBitcoinConfirm {
                    reservation_id: reservation_id.clone(),
                    witness_txid: finalized.witness_txid.clone(),
                },
                created_at_ms: now,
                updated_at_ms: now,
                pending_txid: Some(finalized.witness_txid.clone()),
            };
            if let Err(e) = self.store.ingest_change_utxo(change_utxo).await {
                eprintln!("change re-ingestion failed (continuing): {e}");
            }
        }

        self.pending.write().await.remove(&quote_id);

        Ok(SettlementIntent {
            quote_id,
            maker_id: self.maker_id.clone(),
            status: SettlementStatus::PendingBitcoinConfirm,
            transfer: None,
            expires_at_ms: now_ms() + BROADCAST_CONFIRM_TTL_MS,
            witness_txid: Some(finalized.witness_txid),
            final_consignment: Some(finalized.final_consignment_base64),
        })
    }

    async fn deliver_consignment(
        &self,
        quote_id: QuoteId,
        consignment_base64: String,
    ) -> Result<SettlementIntent, RouterError> {
        self.deliver_consignment_sell(quote_id, consignment_base64)
            .await
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Greedy largest-first selection over the taker's declared-funding UTXOs.
/// Returns the chosen `(outpoint, prevout)` pairs once their values cover
/// `target_sats`, or `None` if the address can't. Mirrors
/// `GreedyLargestFirstSelector`, but operates on the `(Outpoint, TxOut)` shape
/// `BitcoinClient::list_unspent` returns (taker UTXOs aren't maker inventory,
/// so they don't pass through `BtcInventoryStore`).
fn select_btc_inputs(
    available: &[(Outpoint, TxOut)],
    target_sats: u64,
) -> Option<Vec<(Outpoint, TxOut)>> {
    let mut sorted: Vec<&(Outpoint, TxOut)> = available.iter().collect();
    // Largest first; tie-break on outpoint for determinism.
    sorted.sort_by(|a, b| {
        b.1.value_sats
            .cmp(&a.1.value_sats)
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut chosen = Vec::new();
    let mut total = 0u64;
    for pair in sorted {
        if total >= target_sats {
            break;
        }
        total = total.saturating_add(pair.1.value_sats);
        chosen.push(pair.clone());
    }
    (total >= target_sats).then_some(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfq_btc::MockBitcoinClient;
    use rfq_rgb::MockRgbBackend;
    use rfq_types::{AssetId, AssetKind, BitcoinNetwork, ReservationId, RfqId, Side};

    fn synth_outpoint(idx: usize) -> Outpoint {
        Outpoint::new(format!("{idx:064x}"), 0)
    }

    fn maker_id() -> MakerId {
        MakerId("maker-1".to_owned())
    }

    fn asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Rgb20,
            id: "rgb-test-asset".to_owned(),
        }
    }

    fn quote_asset() -> AssetId {
        AssetId {
            network: BitcoinNetwork::Regtest,
            kind: AssetKind::Btc,
            id: "btc".to_owned(),
        }
    }

    fn utxo_with_amount(idx: usize, amount: u64) -> RgbInventoryUtxo {
        RgbInventoryUtxo {
            outpoint: synth_outpoint(idx),
            asset_id: asset(),
            amount,
            btc_sats: 0,
        }
    }

    fn utxo() -> RgbInventoryUtxo {
        utxo_with_amount(0, 100)
    }

    fn inv_utxo(idx: usize, amount: u64, status: InventoryStatus) -> InventoryUtxo {
        let now = now_ms();
        InventoryUtxo {
            outpoint: synth_outpoint(idx),
            asset_id: asset(),
            amount,
            btc_sats: 0,
            status,
            created_at_ms: now,
            updated_at_ms: now,
            pending_txid: None,
        }
    }

    fn maker() -> Maker {
        maker_with_utxos(vec![utxo()])
    }

    fn maker_with_utxos(utxos: Vec<RgbInventoryUtxo>) -> Maker {
        maker_with_btc(utxos, Arc::new(MockBitcoinClient::new()))
    }

    /// Like `maker_with_utxos` but lets a test hold the `MockBitcoinClient` it
    /// passed in, so it can drive broadcast-failure / feerate scenarios.
    fn maker_with_btc(
        utxos: Vec<RgbInventoryUtxo>,
        bitcoin_client: Arc<MockBitcoinClient>,
    ) -> Maker {
        // Declared-funding buy tests resolve the taker's BTC inputs from this
        // address; seed it generously so selection always covers price + fee.
        bitcoin_client.seed_address_unspent(BUY_FUNDING_ADDR, taker_funding());
        let rgb_backend = Arc::new(MockRgbBackend::new(utxos.clone()));
        Maker::new(maker_id(), utxos, rgb_backend, bitcoin_client)
    }

    /// Seed the store directly so tests can pre-set Reserved / Spent statuses.
    fn maker_with_store(rows: Vec<InventoryUtxo>) -> Maker {
        let rgb_backend = Arc::new(MockRgbBackend::new(vec![utxo()]));
        let bitcoin_client = Arc::new(MockBitcoinClient::new());
        bitcoin_client.seed_address_unspent(BUY_FUNDING_ADDR, taker_funding());
        Maker::with_components(
            maker_id(),
            Arc::new(InMemoryInventoryStore::with_seed(rows)),
            Arc::new(GreedyExactFitSelector),
            rgb_backend,
            bitcoin_client,
        )
    }

    /// The BTC address buy-side tests declare as the taker's funding source.
    const BUY_FUNDING_ADDR: &str = "bcrt1qtaker";

    /// One large taker UTXO at `BUY_FUNDING_ADDR`, enough to cover any test
    /// quote's price + fee.
    fn taker_funding() -> Vec<(Outpoint, TxOut)> {
        vec![(
            Outpoint::new(format!("{:064x}", 0xfeedu64), 0),
            TxOut {
                value_sats: 100_000_000,
                script_pubkey: vec![0x00, 0x14],
            },
        )]
    }

    /// Build a `SwapLeg::Buy` accept request for `quote`.
    fn buy_request(quote: &Quote, invoice: &str) -> AcceptQuoteRequest {
        AcceptQuoteRequest {
            quote_id: quote.quote_id.clone(),
            leg: SwapLeg::Buy {
                rgb_invoice: invoice.to_owned(),
                btc_funding_addr: BUY_FUNDING_ADDR.to_owned(),
            },
        }
    }

    fn quote_request(id: &str) -> QuoteRequest {
        QuoteRequest {
            rfq_id: RfqId(id.to_owned()),
            base_asset: asset(),
            quote_asset: quote_asset(),
            side: Side::Buy,
            amount: 100,
            created_at_ms: now_ms(),
        }
    }

    fn sell_quote_request(id: &str) -> QuoteRequest {
        QuoteRequest {
            side: Side::Sell,
            ..quote_request(id)
        }
    }

    // --- sell-side helpers (16c) ---

    /// P2WPKH script — the swap PSBT is segwit-only, so `get_outpoint` rejects
    /// anything else.
    fn sell_p2wpkh() -> Vec<u8> {
        let mut s = vec![0x00, 0x14];
        s.extend(std::iter::repeat_n(0x33, 20));
        s
    }

    /// One of the taker's RGB-bearing outpoints. `sell_bitcoin_client` seeds a
    /// prevout for each so `deliver_consignment`'s lookups resolve.
    fn taker_rgb_outpoint(idx: u64) -> Outpoint {
        Outpoint::new(format!("{:064x}", 0xda7a + idx), 0)
    }

    fn btc_inv(idx: u64, value_sats: u64) -> BtcInventoryUtxo {
        BtcInventoryUtxo {
            outpoint: Outpoint::new(format!("{:064x}", 0xb7c0 + idx), 0),
            value_sats,
            script_pubkey: sell_p2wpkh(),
            status: BtcInventoryStatus::Available,
            created_at_ms: 0,
            updated_at_ms: 0,
            pending_txid: None,
        }
    }

    fn sell_bitcoin_client() -> MockBitcoinClient {
        let mut client = MockBitcoinClient::new();
        for i in 0..3 {
            client = client.with_prevout(
                taker_rgb_outpoint(i),
                TxOut {
                    value_sats: 50_000,
                    script_pubkey: sell_p2wpkh(),
                },
            );
        }
        client
    }

    fn sell_maker_with_btc(client: Arc<MockBitcoinClient>) -> Maker {
        let rgb_backend = Arc::new(MockRgbBackend::new(vec![]));
        Maker::new(maker_id(), vec![], rgb_backend, client)
            .with_btc_inventory((0..3).map(|i| btc_inv(i, 1_000_000)).collect())
    }

    fn sell_maker() -> Maker {
        sell_maker_with_btc(Arc::new(sell_bitcoin_client()))
    }

    fn sell_request(quote: &Quote) -> AcceptQuoteRequest {
        AcceptQuoteRequest {
            quote_id: quote.quote_id.clone(),
            leg: SwapLeg::Sell {
                btc_payout_addr: "bcrt1qtaker000payout".to_owned(),
                rgb_change_invoice: None,
            },
        }
    }

    /// A mock-valid taker consignment against `quote`'s maker RGB invoice.
    fn sell_consignment(quote: &Quote, outpoints: &[Outpoint]) -> String {
        let csv = outpoints
            .iter()
            .map(|o| o.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "mock-consignment|sell|invoice={}|amount={}|outpoints={csv}",
            quote.maker_rgb_invoice.as_deref().unwrap(),
            quote.amount,
        )
    }

    #[tokio::test]
    async fn sell_quote_carries_maker_rgb_invoice() {
        let quote = sell_maker()
            .request_quote(sell_quote_request("rfq-sell"))
            .await
            .unwrap()
            .expect("a BTC-funded maker quotes the sell side");

        // Sell side: the taker needs an invoice to build a consignment against.
        assert!(quote.maker_rgb_invoice.is_some());
    }

    #[tokio::test]
    async fn buy_quote_has_no_maker_rgb_invoice() {
        let quote = maker()
            .request_quote(quote_request("rfq-buy"))
            .await
            .unwrap()
            .expect("quote should be returned");

        assert!(quote.maker_rgb_invoice.is_none());
    }

    #[tokio::test]
    async fn sell_quote_declined_without_btc_inventory() {
        // A buy-only maker has no BTC pool to fund a sell-side payout.
        let quote = maker()
            .request_quote(sell_quote_request("rfq-sell"))
            .await
            .unwrap();
        assert!(quote.is_none());
    }

    #[tokio::test]
    async fn sell_flow_settles_through_accept_consignment_sign() {
        let maker = sell_maker();
        let quote = maker
            .request_quote(sell_quote_request("rfq-sell"))
            .await
            .unwrap()
            .expect("sell quote");

        let accepted = maker
            .accept_quote(quote.clone(), sell_request(&quote))
            .await
            .unwrap();
        assert_eq!(accepted.status, SettlementStatus::AwaitingConsignment);

        let consignment =
            sell_consignment(&quote, &[taker_rgb_outpoint(0), taker_rgb_outpoint(1)]);
        let delivered = maker
            .deliver_consignment(quote.quote_id.clone(), consignment)
            .await
            .unwrap();
        assert_eq!(delivered.status, SettlementStatus::AwaitingTakerSignature);
        let transfer = delivered.transfer.expect("transfer");
        assert!(!transfer.partial_psbt.is_empty());
        // Every input is committed on the sell side — the txid is known now.
        assert!(transfer.expected_witness_txid.is_some());

        let final_intent = maker
            .submit_signed_psbt(
                quote.quote_id.clone(),
                format!("{}|signed", transfer.partial_psbt),
            )
            .await
            .unwrap();
        assert_eq!(final_intent.status, SettlementStatus::PendingBitcoinConfirm);
        assert_eq!(
            final_intent.witness_txid.as_deref(),
            transfer.expected_witness_txid.as_deref(),
            "settled txid matches the one committed at consignment time",
        );
        assert!(final_intent.final_consignment.is_some());

        // The maker's BTC moved to PendingBitcoinConfirm — no longer reserved.
        let snap = maker.btc_store.snapshot().await;
        assert_eq!(snap.reserved_utxos, 0);
        assert_eq!(snap.pending_settlement_utxos, 1);
    }

    #[tokio::test]
    async fn deliver_consignment_rejects_invalid_consignment() {
        let maker = sell_maker();
        let quote = maker
            .request_quote(sell_quote_request("rfq-sell"))
            .await
            .unwrap()
            .expect("sell quote");
        maker
            .accept_quote(quote.clone(), sell_request(&quote))
            .await
            .unwrap();

        let result = maker
            .deliver_consignment(quote.quote_id.clone(), "rgb-invalid".to_owned())
            .await;
        assert!(matches!(result, Err(RouterError::ConsignmentRejected(_))));
        // The BTC reservation was handed back.
        assert_eq!(maker.btc_store.snapshot().await.reserved_utxos, 0);
    }

    #[tokio::test]
    async fn deliver_consignment_after_window_lapse_errors() {
        let maker = sell_maker();
        let quote = maker
            .request_quote(sell_quote_request("rfq-sell"))
            .await
            .unwrap()
            .expect("sell quote");
        maker
            .accept_quote(quote.clone(), sell_request(&quote))
            .await
            .unwrap();

        // Stand in for the cleanup loop expiring the consignment window.
        let reservation = maker
            .btc_store
            .find_reservation_for_quote(&quote.quote_id)
            .await
            .expect("btc reservation live after accept");
        maker
            .btc_store
            .release_reservation(&reservation, now_ms())
            .await
            .unwrap();

        let consignment = sell_consignment(&quote, &[taker_rgb_outpoint(0)]);
        let result = maker
            .deliver_consignment(quote.quote_id.clone(), consignment)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn deliver_consignment_fee_slippage_releases_btc() {
        let maker = sell_maker();
        let mut quote = maker
            .request_quote(sell_quote_request("rfq-sell"))
            .await
            .unwrap()
            .expect("sell quote");
        // Tiny baseline + zero tolerance: the re-estimate at consignment time
        // (5 sat/vB × 200 vB = 1000) blows the cap.
        quote.estimated_fee_sats = 1;
        quote.fee_slippage_bps = 0;
        maker
            .accept_quote(quote.clone(), sell_request(&quote))
            .await
            .unwrap();

        let consignment = sell_consignment(&quote, &[taker_rgb_outpoint(0)]);
        let result = maker
            .deliver_consignment(quote.quote_id.clone(), consignment)
            .await;
        assert!(matches!(
            result,
            Err(RouterError::FeeSlippageExceeded { .. })
        ));
        assert_eq!(maker.btc_store.snapshot().await.reserved_utxos, 0);
    }

    #[tokio::test]
    async fn sell_sign_rejects_bait_and_switch() {
        let maker = sell_maker();
        let quote = maker
            .request_quote(sell_quote_request("rfq-sell"))
            .await
            .unwrap()
            .expect("sell quote");
        maker
            .accept_quote(quote.clone(), sell_request(&quote))
            .await
            .unwrap();
        let consignment =
            sell_consignment(&quote, &[taker_rgb_outpoint(0), taker_rgb_outpoint(1)]);
        let delivered = maker
            .deliver_consignment(quote.quote_id.clone(), consignment)
            .await
            .unwrap();
        let psbt = delivered.transfer.expect("transfer").partial_psbt;

        // The taker drops one consigned outpoint from the signed PSBT.
        let tampered = psbt.replace(&taker_rgb_outpoint(0).to_string(), "");
        let result = maker
            .submit_signed_psbt(quote.quote_id.clone(), format!("{tampered}|signed"))
            .await;
        assert!(matches!(result, Err(RouterError::PsbtInvalid(_))));
        assert_eq!(maker.btc_store.snapshot().await.reserved_utxos, 0);
    }

    #[tokio::test]
    async fn sell_sign_rejects_txid_mismatch() {
        let maker = sell_maker();
        let quote = maker
            .request_quote(sell_quote_request("rfq-sell"))
            .await
            .unwrap()
            .expect("sell quote");
        maker
            .accept_quote(quote.clone(), sell_request(&quote))
            .await
            .unwrap();
        let consignment = sell_consignment(&quote, &[taker_rgb_outpoint(0)]);
        let delivered = maker
            .deliver_consignment(quote.quote_id.clone(), consignment)
            .await
            .unwrap();
        let transfer = delivered.transfer.expect("transfer");
        let expected_wt = transfer.expected_witness_txid.expect("committed txid");

        // Tamper the committed witness txid — the inputs stay intact, so the
        // bait-and-switch guard passes and the txid check is what fires.
        let tampered = transfer.partial_psbt.replace(&expected_wt, &"f".repeat(64));
        let result = maker
            .submit_signed_psbt(quote.quote_id.clone(), format!("{tampered}|signed"))
            .await;
        assert!(matches!(result, Err(RouterError::PsbtInvalid(_))));
        assert_eq!(maker.btc_store.snapshot().await.reserved_utxos, 0);
    }

    #[tokio::test]
    async fn sell_sign_broadcast_failure_releases_btc() {
        let client = Arc::new(sell_bitcoin_client());
        let maker = sell_maker_with_btc(client.clone());
        let quote = maker
            .request_quote(sell_quote_request("rfq-sell"))
            .await
            .unwrap()
            .expect("sell quote");
        maker
            .accept_quote(quote.clone(), sell_request(&quote))
            .await
            .unwrap();
        let consignment = sell_consignment(&quote, &[taker_rgb_outpoint(0)]);
        let delivered = maker
            .deliver_consignment(quote.quote_id.clone(), consignment)
            .await
            .unwrap();
        let psbt = delivered.transfer.expect("transfer").partial_psbt;

        client.fail_next_broadcast("simulated rpc failure");
        let result = maker
            .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
            .await;
        assert!(result.is_err());
        assert_eq!(maker.btc_store.snapshot().await.reserved_utxos, 0);
    }

    #[tokio::test]
    async fn first_quote_reserves_allocation() {
        let maker = maker();

        let quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();

        let quote = quote.expect("quote should be returned");
        let utxos = maker.utxo_snapshot().await;
        assert_eq!(utxos.len(), 1);
        assert!(matches!(
            &utxos[0].status,
            InventoryStatus::Reserved { quote_id, expires_at_ms, .. }
                if quote_id == &quote.quote_id && *expires_at_ms == quote.expires_at_ms
        ));
    }

    #[tokio::test]
    async fn inventory_summary_counts_initial_inventory_as_available() {
        let maker = maker_with_utxos(vec![
            utxo_with_amount(0, 100),
            utxo_with_amount(1, 200),
        ]);

        let snapshot = maker.inventory_summary().await;

        assert_eq!(
            snapshot,
            InventorySnapshot {
                total_amount: 300,
                available_amount: 300,
                reserved_amount: 0,
                spent_amount: 0,
                total_allocations: 2,
                available_allocations: 2,
                reserved_allocations: 0,
                spent_allocations: 0,
            }
        );
    }

    #[tokio::test]
    async fn inventory_summary_counts_reserved_after_quote() {
        let maker = maker();

        let quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();
        let snapshot = maker.inventory_summary().await;

        assert!(quote.is_some());
        assert_eq!(snapshot.available_amount, 0);
        assert_eq!(snapshot.reserved_amount, 100);
        assert_eq!(snapshot.spent_amount, 0);
        assert_eq!(snapshot.available_allocations, 0);
        assert_eq!(snapshot.reserved_allocations, 1);
        assert_eq!(snapshot.spent_allocations, 0);
    }

    #[tokio::test]
    async fn inventory_summary_releases_expired_reservation() {
        let maker = maker_with_store(vec![inv_utxo(
            0,
            100,
            InventoryStatus::Reserved {
                reservation_id: ReservationId("res-0".to_owned()),
                rfq_id: RfqId("rfq-0".to_owned()),
                quote_id: QuoteId("expired-quote".to_owned()),
                expires_at_ms: 0,
            },
        )]);

        let snapshot = maker.inventory_summary().await;
        let utxos = maker.utxo_snapshot().await;

        assert_eq!(snapshot.available_amount, 100);
        assert_eq!(snapshot.reserved_amount, 0);
        assert_eq!(snapshot.available_allocations, 1);
        assert_eq!(snapshot.reserved_allocations, 0);
        assert!(matches!(utxos[0].status, InventoryStatus::Available));
    }

    #[tokio::test]
    async fn release_expired_reservations_returns_count_and_releases() {
        let maker = maker_with_store(vec![inv_utxo(
            0,
            100,
            InventoryStatus::Reserved {
                reservation_id: ReservationId("res-0".to_owned()),
                rfq_id: RfqId("rfq-0".to_owned()),
                quote_id: QuoteId("expired-quote".to_owned()),
                expires_at_ms: 0,
            },
        )]);

        let released = maker.release_expired_reservations().await;
        let utxos = maker.utxo_snapshot().await;

        assert_eq!(released, 1);
        assert!(matches!(utxos[0].status, InventoryStatus::Available));
    }

    #[tokio::test]
    async fn release_expired_reservations_ignores_active_and_spent() {
        let maker = maker_with_store(vec![
            inv_utxo(
                0,
                100,
                InventoryStatus::Reserved {
                    reservation_id: ReservationId("res-0".to_owned()),
                    rfq_id: RfqId("rfq-0".to_owned()),
                    quote_id: QuoteId("active-quote".to_owned()),
                    expires_at_ms: now_ms() + 30_000,
                },
            ),
            inv_utxo(
                1,
                100,
                InventoryStatus::Spent {
                    witness_txid: "wt-1".to_owned(),
                    quote_id: QuoteId("spent-quote".to_owned()),
                },
            ),
        ]);

        let released = maker.release_expired_reservations().await;
        let utxos = maker.utxo_snapshot().await;

        assert_eq!(released, 0);
        assert!(matches!(
            utxos[0].status,
            InventoryStatus::Reserved { .. }
        ));
        assert!(matches!(utxos[1].status, InventoryStatus::Spent { .. }));
    }

    #[tokio::test]
    async fn second_quote_cannot_reuse_reserved_liquidity() {
        let maker = maker();

        let first_quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();
        let second_quote = maker.request_quote(quote_request("rfq-2")).await.unwrap();

        assert!(first_quote.is_some());
        assert!(second_quote.is_none());
    }

    #[tokio::test]
    async fn expired_reservation_becomes_available() {
        let expired_quote_id = QuoteId("expired-quote".to_owned());
        let maker = maker_with_store(vec![inv_utxo(
            0,
            100,
            InventoryStatus::Reserved {
                reservation_id: ReservationId("res-0".to_owned()),
                rfq_id: RfqId("rfq-0".to_owned()),
                quote_id: expired_quote_id,
                expires_at_ms: 0,
            },
        )]);

        let quote = maker.request_quote(quote_request("rfq-1")).await.unwrap();

        assert!(quote.is_some());
        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(
            &utxos[0].status,
            InventoryStatus::Reserved { quote_id, .. } if quote_id == &quote.unwrap().quote_id
        ));
    }

    #[tokio::test]
    async fn accept_keeps_utxo_reserved_awaiting_signature() {
        // Under the swap flow, `accept` no longer spends — it builds the PSBT
        // and waits for the taker's signature. The reserved UTXO stays
        // `Reserved` (with its deadline extended to the settlement window).
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote should be returned");

        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:test_invoice"))
            .await
            .unwrap();

        assert_eq!(intent.status, SettlementStatus::AwaitingTakerSignature);
        assert!(intent.transfer.is_some());
        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(
            &utxos[0].status,
            InventoryStatus::Reserved { quote_id, .. } if quote_id == &quote.quote_id
        ));
    }

    #[tokio::test]
    async fn inventory_counts_pending_confirm_after_sign() {
        // accept → sign drives the input UTXO to `PendingBitcoinConfirm`
        // (broadcast, awaiting confirmation) — not `Spent`, which only happens
        // once the witness tx confirms.
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote should be returned");

        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:test_invoice"))
            .await
            .unwrap();
        let psbt = intent.transfer.expect("transfer").partial_psbt;
        maker
            .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
            .await
            .unwrap();

        let ext = maker.extended_inventory_summary().await;
        assert_eq!(ext.available_utxos, 0);
        assert_eq!(ext.reserved_utxos, 0);
        assert_eq!(ext.pending_settlement_utxos, 1);
        assert_eq!(ext.pending_settlement_amount, 100);
    }

    #[tokio::test]
    async fn spent_allocation_cannot_be_quoted_again() {
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote should be returned");
        maker
            .accept_quote(
                quote.clone(),
                AcceptQuoteRequest {
                    quote_id: quote.quote_id.clone(),
                    leg: SwapLeg::Buy {
                        rgb_invoice: "rgb:test_invoice".to_owned(),
                        btc_funding_addr: "bcrt1qtaker".to_owned(),
                    },
                },
            )
            .await
            .unwrap();

        let next_quote = maker.request_quote(quote_request("rfq-2")).await.unwrap();

        assert!(next_quote.is_none());
    }

    // --- new 14c tests ---

    #[tokio::test]
    async fn request_quote_reserves_specific_utxos_not_whole_inventory() {
        // Three available UTXOs of 100 each. A request for 80 reserves exactly
        // one of them — the other two stay Available. This is the per-UTXO
        // win over the pre-#14 whole-allocation model.
        let maker = maker_with_utxos(vec![
            utxo_with_amount(0, 100),
            utxo_with_amount(1, 100),
            utxo_with_amount(2, 100),
        ]);

        let mut request = quote_request("rfq-1");
        request.amount = 80;
        let quote = maker.request_quote(request).await.unwrap();
        assert!(quote.is_some());

        let ext = maker.extended_inventory_summary().await;
        assert_eq!(ext.available_utxos, 2);
        assert_eq!(ext.reserved_utxos, 1);
        assert_eq!(ext.available_amount, 200);
        assert_eq!(ext.reserved_amount, 100);
    }

    #[tokio::test]
    async fn expired_reservation_releases_per_utxo() {
        let maker = maker_with_store(vec![
            inv_utxo(
                0,
                100,
                InventoryStatus::Reserved {
                    reservation_id: ReservationId("res-0".to_owned()),
                    rfq_id: RfqId("rfq-0".to_owned()),
                    quote_id: QuoteId("expired".to_owned()),
                    expires_at_ms: 0,
                },
            ),
            inv_utxo(1, 200, InventoryStatus::Available),
        ]);

        let released = maker.release_expired_reservations().await;
        let ext = maker.extended_inventory_summary().await;

        assert_eq!(released, 1);
        assert_eq!(ext.available_utxos, 2);
        assert_eq!(ext.reserved_utxos, 0);
    }

    #[tokio::test]
    async fn concurrent_request_quotes_do_not_double_reserve() {
        // Five UTXOs of 100 each. Spawn 10 concurrent quote requests of 50;
        // at most 5 should succeed (one UTXO per quote with WholeUtxoSelector).
        let maker = Arc::new(maker_with_utxos(vec![
            utxo_with_amount(0, 100),
            utxo_with_amount(1, 100),
            utxo_with_amount(2, 100),
            utxo_with_amount(3, 100),
            utxo_with_amount(4, 100),
        ]));

        let mut handles = Vec::new();
        for i in 0..10 {
            let maker = maker.clone();
            handles.push(tokio::spawn(async move {
                let mut request = quote_request(&format!("rfq-{i}"));
                request.amount = 50;
                maker.request_quote(request).await.unwrap()
            }));
        }

        let mut successes = 0;
        for h in handles {
            if h.await.unwrap().is_some() {
                successes += 1;
            }
        }
        assert_eq!(successes, 5);

        let ext = maker.extended_inventory_summary().await;
        assert_eq!(ext.reserved_utxos, 5);
        assert_eq!(ext.available_utxos, 0);
    }

    // --- 14e tests ---

    #[tokio::test]
    async fn rebalance_emits_no_triggers_for_healthy_inventory() {
        // Single big UTXO → fragmentation 0.0, available_utxos = 1 (between
        // min and max). All triggers should be silent.
        let maker = maker_with_utxos(vec![utxo_with_amount(0, 1000)]);
        let policy = RebalancePolicy {
            fragmentation_threshold: 0.7,
            max_utxo_count: 50,
            min_utxo_count: 1,
        };
        let plan = maker.rebalance(&policy).await;
        assert!(plan.is_empty(), "expected no triggers, got {plan:?}");
    }

    #[tokio::test]
    async fn rebalance_fires_high_fragmentation_above_threshold() {
        // 5 UTXOs of 100 each. Largest 100 / total 500 = 0.2 share →
        // fragmentation = 0.8, which crosses 0.7.
        let maker = maker_with_utxos(vec![
            utxo_with_amount(0, 100),
            utxo_with_amount(1, 100),
            utxo_with_amount(2, 100),
            utxo_with_amount(3, 100),
            utxo_with_amount(4, 100),
        ]);
        let policy = RebalancePolicy {
            fragmentation_threshold: 0.7,
            max_utxo_count: 50,
            min_utxo_count: 1,
        };
        let plan = maker.rebalance(&policy).await;
        assert!(plan
            .triggers
            .iter()
            .any(|t| matches!(t, RebalanceTrigger::HighFragmentation { .. })));
    }

    #[tokio::test]
    async fn rebalance_fires_too_many_utxos_above_max() {
        let utxos: Vec<_> = (0..10).map(|i| utxo_with_amount(i, 10)).collect();
        let maker = maker_with_utxos(utxos);
        let policy = RebalancePolicy {
            fragmentation_threshold: 1.0, // suppress fragmentation trigger
            max_utxo_count: 5,
            min_utxo_count: 1,
        };
        let plan = maker.rebalance(&policy).await;
        assert!(plan.triggers.iter().any(|t| matches!(
            t,
            RebalanceTrigger::TooManyUtxos { count: 10, max: 5 }
        )));
    }

    #[tokio::test]
    async fn rebalance_fires_too_few_utxos_below_min() {
        let maker = maker_with_utxos(vec![utxo_with_amount(0, 1000)]);
        let policy = RebalancePolicy {
            fragmentation_threshold: 1.0,
            max_utxo_count: 50,
            min_utxo_count: 3,
        };
        let plan = maker.rebalance(&policy).await;
        assert!(plan.triggers.iter().any(|t| matches!(
            t,
            RebalanceTrigger::TooFewUtxos { count: 1, min: 3 }
        )));
    }

    #[tokio::test]
    async fn sign_with_change_ingests_pending_bitcoin_confirm_utxo() {
        // Single UTXO of 100; request 60 → change = 40. Change re-ingestion
        // happens at `/sign` (post-broadcast), not at accept. After sign, the
        // input UTXO is PendingBitcoinConfirm and a synthetic change UTXO of
        // 40 has been ingested, also PendingBitcoinConfirm.
        let maker = maker_with_utxos(vec![utxo_with_amount(0, 100)]);

        let mut request = quote_request("rfq-1");
        request.amount = 60;
        let quote = maker
            .request_quote(request)
            .await
            .unwrap()
            .expect("quote should be issued");

        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:test"))
            .await
            .unwrap();
        let psbt = intent.transfer.expect("transfer").partial_psbt;
        maker
            .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
            .await
            .unwrap();

        let utxos = maker.utxo_snapshot().await;
        let pending: Vec<_> = utxos
            .iter()
            .filter(|u| matches!(u.status, InventoryStatus::PendingBitcoinConfirm { .. }))
            .collect();
        // Input (100) + change (40), both PendingBitcoinConfirm.
        assert_eq!(pending.len(), 2);
        let change = pending
            .iter()
            .find(|u| u.amount == 40)
            .expect("expected a change UTXO");
        assert!(change.pending_txid.is_some());
    }

    #[tokio::test]
    async fn buy_flow_settles_through_accept_then_sign() {
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote should be returned");

        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
            .await
            .unwrap();
        assert_eq!(intent.status, SettlementStatus::AwaitingTakerSignature);
        let transfer = intent.transfer.expect("transfer");
        assert!(transfer.consignment.is_some());

        let final_intent = maker
            .submit_signed_psbt(
                quote.quote_id.clone(),
                format!("{}|signed", transfer.partial_psbt),
            )
            .await
            .unwrap();
        assert_eq!(final_intent.status, SettlementStatus::PendingBitcoinConfirm);
        assert!(final_intent.witness_txid.is_some());
        assert!(final_intent.final_consignment.is_some());

        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(
            utxos[0].status,
            InventoryStatus::PendingBitcoinConfirm { .. }
        ));
    }

    #[tokio::test]
    async fn submit_signed_psbt_unknown_quote_errors() {
        let maker = maker();
        let result = maker
            .submit_signed_psbt(QuoteId("no-such-quote".to_owned()), "psbt".to_owned())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn submit_signed_psbt_rejects_unsigned_psbt() {
        // MockRgbBackend::finalize_after_taker_sign rejects an empty PSBT.
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote");
        maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
            .await
            .unwrap();

        let result = maker
            .submit_signed_psbt(quote.quote_id.clone(), String::new())
            .await;
        assert!(matches!(result, Err(RouterError::PsbtInvalid(_))));
        // Reservation released back to Available.
        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(utxos[0].status, InventoryStatus::Available));
    }

    #[tokio::test]
    async fn buy_accept_fee_slippage_releases_reservation() {
        // Default mock feerate (5 sat/vB) × 200 vB = 1000 sat actual. Crafting
        // the quote with a tiny baseline + zero slippage tolerance makes the
        // accept-time estimate blow the cap. Under declared-funding the buy
        // PSBT is fully committed at accept, so slippage is checked there (not
        // at /sign).
        let maker = maker();
        let mut quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote");
        quote.estimated_fee_sats = 1;
        quote.fee_slippage_bps = 0;

        let result = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
            .await;
        assert!(matches!(
            result,
            Err(RouterError::FeeSlippageExceeded { .. })
        ));
        // Reservation released back to Available.
        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(utxos[0].status, InventoryStatus::Available));
    }

    #[tokio::test]
    async fn submit_signed_psbt_broadcast_failure_releases_reservation() {
        let btc = Arc::new(MockBitcoinClient::new());
        let maker = maker_with_btc(vec![utxo()], btc.clone());
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote");
        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
            .await
            .unwrap();
        let psbt = intent.transfer.expect("transfer").partial_psbt;

        btc.fail_next_broadcast("simulated rpc failure");
        let result = maker
            .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
            .await;
        assert!(result.is_err());
        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(utxos[0].status, InventoryStatus::Available));
    }

    #[tokio::test]
    async fn submit_signed_psbt_after_settlement_window_lapse_errors() {
        // Simulate the taker missing the TAKER_SIGNATURE_TTL_MS window: once
        // the cleanup loop has released the reservation, submit_signed_psbt
        // must refuse, drop the stale pending entry, and leave the maker's
        // UTXO back on Available — no broadcast.
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote");
        let intent = maker
            .accept_quote(quote.clone(), buy_request(&quote, "rgb:invoice"))
            .await
            .unwrap();
        let psbt = intent.transfer.expect("transfer").partial_psbt;

        // Stand in for the cleanup loop expiring the settlement window.
        let reservation_id = maker
            .store
            .find_reservation_for_quote(&quote.quote_id)
            .await
            .expect("reservation live after accept");
        maker
            .store
            .release_reservation(&reservation_id, now_ms())
            .await
            .unwrap();

        let result = maker
            .submit_signed_psbt(quote.quote_id.clone(), format!("{psbt}|signed"))
            .await;
        assert!(result.is_err());
        let utxos = maker.utxo_snapshot().await;
        assert!(matches!(utxos[0].status, InventoryStatus::Available));
    }

    #[tokio::test]
    async fn accept_releases_reservation_on_transfer_failure() {
        // MockRgbBackend rejects invoices that don't start with "rgb:".
        let maker = maker();
        let quote = maker
            .request_quote(quote_request("rfq-1"))
            .await
            .unwrap()
            .expect("quote should be issued");

        let result = maker
            .accept_quote(
                quote.clone(),
                AcceptQuoteRequest {
                    quote_id: quote.quote_id,
                    leg: SwapLeg::Buy {
                        rgb_invoice: "not-an-invoice".into(),
                        btc_funding_addr: "bcrt1qtaker".to_owned(),
                    },
                },
            )
            .await;
        assert!(result.is_err());

        let snapshot = maker.inventory_summary().await;
        assert_eq!(snapshot.available_amount, 100);
        assert_eq!(snapshot.reserved_amount, 0);
        assert_eq!(snapshot.spent_amount, 0);
    }

    #[tokio::test]
    async fn legacy_inventory_summary_matches_extended_downcast() {
        let maker = maker_with_store(vec![
            inv_utxo(0, 100, InventoryStatus::Available),
            inv_utxo(
                1,
                200,
                InventoryStatus::Reserved {
                    reservation_id: ReservationId("res-1".to_owned()),
                    rfq_id: RfqId("rfq-1".to_owned()),
                    quote_id: QuoteId("q".to_owned()),
                    expires_at_ms: now_ms() + 60_000,
                },
            ),
        ]);

        let legacy = maker.inventory_summary().await;
        let ext = maker.extended_inventory_summary().await;
        let derived: InventorySnapshot = (&ext).into();
        assert_eq!(legacy, derived);
    }
}
