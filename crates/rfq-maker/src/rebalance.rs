//! Rebalancing planner — denomination-ladder types + the pure functions that
//! decide what to split. Two consumers: the standalone rebalance executor
//! ([`plan_ladder`] + [`assemble_rebalance_tx`]) and settlement-piggyback
//! ([`plan_change_rungs`]). All pure — no `Maker`/store/chain access — so they
//! unit-test in isolation (see the tests in `lib.rs`). Re-exported from the
//! crate root, so external call sites use `rfq_maker::plan_ladder` etc.

use rfq_types::{AssetId, Outpoint};

/// Every RGB-bearing output carries exactly this much BTC dust, independent of
/// the RGB amount it holds — a 1-unit rung and a 1M-unit rung both anchor on 546
/// sats. Mirrors `swap::SEAL_ANCHOR_SATS`; used by the rebalance budget.
pub const REBALANCE_ANCHOR_SATS: u64 = 546;

/// Sentinel `RfqId`/`QuoteId` prefix for the internal reservations the rebalance
/// executor makes. Never collides with broker-issued ids, so the quoting path
/// and the rebalancer can't fight over the same UTXO.
pub const REBALANCE_RFQ_ID: &str = "__rebalance__";

/// Smallest *spendable BTC* piece the BTC ladder will create (and the floor for
/// split change — sub-floor change is folded into the fee). Distinct from the
/// 546-sat anchor: that's dust on an RGB-bearing output; this is the minimum for
/// a plain BTC UTXO the maker pays sell-side takers from.
pub const DEFAULT_BTC_MIN_PIECE_SATS: u64 = 1000;

/// RGB ladder `min_piece` default — the transferred asset amount doesn't affect
/// cost (every rung anchors on 546 sats regardless), so any positive amount is a
/// valid rung.
pub const DEFAULT_RGB_MIN_PIECE: u64 = 1;

/// A target distribution of UTXO values, largest→smallest, so coin selection and
/// tx-building stay cheap. Geometric: tier `i` targets `base * ratio^i` and wants
/// `copies` pieces of it; tiers whose value falls below `min_piece` are dropped.
/// `ratio` is expected in `(0, 1)` (a halving ladder is `0.5`).
#[derive(Debug, Clone, PartialEq)]
pub struct LadderSpec {
    pub base: u64,
    pub ratio: f64,
    pub tiers: u32,
    pub copies: u32,
    pub min_piece: u64,
}

impl LadderSpec {
    /// Expand into the desired multiset of rung values, largest first — `copies`
    /// of each tier whose value is `>= min_piece` (and non-zero).
    pub fn target_rungs(&self) -> Vec<u64> {
        let mut out = Vec::new();
        let mut v = self.base as f64;
        for _ in 0..self.tiers {
            let rung = v as u64;
            if rung >= self.min_piece && rung > 0 {
                for _ in 0..self.copies {
                    out.push(rung);
                }
            }
            v *= self.ratio;
        }
        out
    }
}

/// One contract's rungs to carve from a single fat source UTXO.
#[derive(Debug, Clone, PartialEq)]
pub struct AssetSplit {
    pub asset: AssetId,
    pub source: Outpoint,
    /// RGB amount held by the source allocation.
    pub source_amount: u64,
    /// The source UTXO's own BTC value — funds its share of the anchor outputs.
    pub source_btc_sats: u64,
    /// RGB amounts to carve, largest→smallest (remainder = `source_amount - Σ`).
    pub rungs: Vec<u64>,
}

/// The BTC pool's rungs to carve from a single fat BTC source UTXO.
#[derive(Debug, Clone, PartialEq)]
pub struct BtcSplit {
    pub source: Outpoint,
    pub source_sats: u64,
    /// Sats to carve, largest→smallest.
    pub rungs: Vec<u64>,
}

/// The combined plan for one rebalance pass: every asset's rungs + the BTC
/// rungs, carved by a single transaction with one fee and one tapret commitment.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RebalanceTx {
    pub assets: Vec<AssetSplit>,
    pub btc: Option<BtcSplit>,
    pub fee_sats: u64,
    /// BTC the tx must source: `fee + anchor·(rgb outputs) + Σ(btc rungs)`.
    pub btc_needed: u64,
}

/// Greedily match available pieces to target rungs (a piece covers a target iff
/// `piece >= target`). Returns the targets left UNcovered — the rungs to mint.
/// Classic "assign cookies" greedy: sort both ascending, satisfy the smallest
/// target with the smallest sufficient piece. Optimal for maximizing coverage.
pub fn ladder_deficit(available: &[u64], targets: &[u64]) -> Vec<u64> {
    let mut pieces: Vec<u64> = available.to_vec();
    pieces.sort_unstable();
    let mut tgts: Vec<u64> = targets.to_vec();
    tgts.sort_unstable();
    let mut deficit = Vec::new();
    let mut pi = 0usize;
    for t in tgts {
        while pi < pieces.len() && pieces[pi] < t {
            pi += 1;
        }
        if pi < pieces.len() {
            pi += 1; // consume this piece for the target
        } else {
            deficit.push(t);
        }
    }
    deficit
}

/// Decide whether `available` `(outpoint, value)` needs splitting toward `spec`,
/// and if so return the fattest source to cut plus the rung amounts to carve
/// (largest first). Pure & idempotent — feeding the post-split distribution back
/// returns `None`. At most ONE source per call (bounds tx cost; the loop
/// converges over successive passes). Works for both RGB (asset units) and BTC
/// (sats) since it's value-agnostic.
pub fn plan_ladder(
    available: &[(Outpoint, u64)],
    spec: &LadderSpec,
) -> Option<(Outpoint, u64, Vec<u64>)> {
    let targets = spec.target_rungs();
    if targets.is_empty() {
        return None;
    }
    let values: Vec<u64> = available.iter().map(|(_, v)| *v).collect();
    let mut deficit = ladder_deficit(&values, &targets);
    if deficit.is_empty() {
        return None;
    }
    // Source = the fattest available UTXO (the reserve we cut rungs from).
    let (source_op, source_val) = available.iter().cloned().max_by_key(|(_, v)| *v)?;
    // Carve deficit rungs largest-first that fit, always leaving a positive
    // remainder (>= min_piece) so the host output carries a real allocation.
    deficit.sort_unstable_by(|a, b| b.cmp(a));
    let reserve = spec.min_piece.max(1);
    let mut rungs = Vec::new();
    let mut remaining = source_val;
    for d in deficit {
        if remaining >= d.saturating_add(reserve) {
            rungs.push(d);
            remaining -= d;
        }
    }
    if rungs.is_empty() {
        return None; // source too small to carve anything useful
    }
    Some((source_op, source_val, rungs))
}

/// Rungs to carve out of a swap's RGB `change` to piggyback a ladder split onto
/// the settlement tx (no dedicated rebalance tx, no extra fee). `available` is
/// the asset's CURRENT Available pieces — the change isn't a UTXO yet, so it's
/// excluded. Returns the deficit rungs (largest-first) that fit within `change`
/// while leaving a remainder; empty when the change can't fund even one rung
/// (the "change too small → skip" rule). Mirrors [`plan_ladder`]'s carve with the
/// swap change as the source. The caller separately caps the count by the BTC
/// available to anchor the rung outputs.
pub fn plan_change_rungs(available: &[u64], change: u64, spec: &LadderSpec) -> Vec<u64> {
    let targets = spec.target_rungs();
    if targets.is_empty() || change == 0 {
        return Vec::new();
    }
    let mut deficit = ladder_deficit(available, &targets);
    deficit.sort_unstable_by(|a, b| b.cmp(a));
    let reserve = spec.min_piece.max(1);
    let mut rungs = Vec::new();
    let mut remaining = change;
    for d in deficit {
        if remaining >= d.saturating_add(reserve) {
            rungs.push(d);
            remaining -= d;
        }
    }
    rungs
}

/// BTC the rebalance tx must source for a given set of split plans.
fn required_btc(assets: &[AssetSplit], btc: &Option<BtcSplit>, fee_sats: u64) -> u64 {
    // Each asset emits one output per rung + one remainder output, each needing
    // an anchor's worth of dust.
    let rgb_outputs: u64 = assets.iter().map(|a| a.rungs.len() as u64 + 1).sum();
    let btc_rungs: u64 = btc.as_ref().map(|b| b.rungs.iter().sum()).unwrap_or(0);
    fee_sats
        .saturating_add(REBALANCE_ANCHOR_SATS.saturating_mul(rgb_outputs))
        .saturating_add(btc_rungs)
}

/// Drop one rung to shrink the BTC requirement. RGB rungs go first (smallest
/// value across all assets — each frees one anchor while keeping the big, useful
/// pieces); once no RGB rungs remain, the smallest BTC rung goes. Empty asset /
/// BTC plans are removed. Returns `false` when nothing is left to drop.
fn scale_down(assets: &mut Vec<AssetSplit>, btc: &mut Option<BtcSplit>) -> bool {
    let mut smallest: Option<(usize, usize, u64)> = None; // (asset, rung, value)
    for (ai, a) in assets.iter().enumerate() {
        for (ri, &r) in a.rungs.iter().enumerate() {
            if smallest.is_none_or(|(_, _, sv)| r < sv) {
                smallest = Some((ai, ri, r));
            }
        }
    }
    if let Some((ai, ri, _)) = smallest {
        assets[ai].rungs.remove(ri);
        assets.retain(|a| !a.rungs.is_empty());
        return true;
    }
    if let Some(b) = btc {
        if let Some((ri, _)) = b.rungs.iter().enumerate().min_by_key(|(_, v)| **v) {
            b.rungs.remove(ri);
            if b.rungs.is_empty() {
                *btc = None;
            }
            return true;
        }
    }
    false
}

/// Combine per-asset + BTC split plans into one rebalance-tx plan under a single
/// BTC budget. `btc_available_sats` is what the executor will commit to fund the
/// tx (the BTC source plus any extra fee inputs). When the budget is short, rungs
/// are dropped (RGB first — see [`scale_down`]) until it fits; if it can't even
/// fund the fee + remainders, returns `None` (caller warns & skips — the maker
/// can't mint BTC). Returns `None` when there's nothing to split.
pub fn assemble_rebalance_tx(
    mut assets: Vec<AssetSplit>,
    mut btc: Option<BtcSplit>,
    btc_available_sats: u64,
    fee_sats: u64,
) -> Option<RebalanceTx> {
    assets.retain(|a| !a.rungs.is_empty());
    if btc.as_ref().is_some_and(|b| b.rungs.is_empty()) {
        btc = None;
    }
    if assets.is_empty() && btc.is_none() {
        return None;
    }
    loop {
        if assets.is_empty() && btc.is_none() {
            return None; // scaled everything away — nothing left to split
        }
        // Recompute each pass: dropping a whole asset removes its source UTXO
        // from the inputs, so its `source_btc_sats` no longer helps fund anchors.
        let rgb_source_sats: u64 = assets.iter().map(|a| a.source_btc_sats).sum();
        let avail = btc_available_sats.saturating_add(rgb_source_sats);
        let need = required_btc(&assets, &btc, fee_sats);
        if avail >= need {
            return Some(RebalanceTx {
                assets,
                btc,
                fee_sats,
                btc_needed: need,
            });
        }
        if !scale_down(&mut assets, &mut btc) {
            return None;
        }
    }
}

/// Rough vsize (vBytes) of an all-P2TR keyspend rebalance tx with the given
/// input/output counts: ~11 tx overhead + ~58/input + ~43/output. The tapret
/// commitment rides the host output's taproot key (keyspend), so it needs no
/// extra witness budget. Multiplied by the next-block feerate to size the fee.
pub fn estimate_rebalance_vbytes(num_inputs: usize, num_outputs: usize) -> u64 {
    11 + num_inputs as u64 * 58 + num_outputs as u64 * 43
}
