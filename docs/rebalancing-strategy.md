# Maker Inventory Rebalancing Strategy

This document is the design rationale behind the rebalance scaffolding that lands in [#14e](broker-maker-node-roadmap.md). The 14e PR ships the *monitoring* half — metrics, a `RebalancePlan` computation, and a periodic logging loop. The *execution* half (actually broadcasting rebalance transactions) is deferred to a follow-up issue.

If you only read one section, read [Why we picked (C)](#why-we-picked-c).

---

## Fragmentation primer

In a UTXO model, every input you spend costs bytes (signatures, witness data, index lookups) and those bytes cost fees. A maker holding 1000 units of an RGB asset across **ten** 100-unit UTXOs needs at least six inputs to settle a 600-unit RFQ. The same maker holding those 1000 units in **one** UTXO needs one input plus change. The fee delta scales linearly with input count, and the *largest* single RFQ a maker can fill is capped by the sum of the largest N UTXOs that fit on-chain — a fragmented inventory has a hard ceiling on max-fillable size.

Concrete cost example, at 50 sat/vB and ~150 vbytes per RGB-aware input:
- One 1000-unit UTXO → 1 input → ~150 vbytes → ~7,500 sat fee
- Ten 100-unit UTXOs → 10 inputs → ~1,500 vbytes → ~75,000 sat fee

Same RFQ outcome. **10× the fee.** That's why fragmentation matters.

## The `fragmentation_score` formula

```
fragmentation_score = 1.0 - (largest_available / total_available)
```

Computed in [ExtendedInventorySnapshot::from_utxos](../crates/rfq-types/src/lib.rs).

Range: `0.0` (one UTXO holds everything available — perfect) to approaching `1.0` (dust spread evenly across many UTXOs — bad).

Worked examples:

| Inventory | `largest` | `total` | `fragmentation_score` | reading |
|---|---|---|---|---|
| `{1000}` | 1000 | 1000 | **0.00** | one fat UTXO — perfect |
| `{500, 500}` | 500 | 1000 | **0.50** | two equal — fine for small RFQs |
| `{500, 100, 100, 100, 100, 100}` | 500 | 1000 | **0.50** | same score, worse in practice |
| `{100} × 10` | 100 | 1000 | **0.90** | dust — rebalance candidate |

### Known limitation

The simple formula can't distinguish `{500, 500}` from `{500, 100 × 5}` — both score `0.50`. As a *trigger* metric it's fine; if we need sharper signal later we'd switch to **effective UTXO count** (`total² / sum(amount²)`, an inverse Herfindahl) or read `average_input_count` over recent settlements directly. Both of those fields are already in `ExtendedInventorySnapshot`, fed externally by the maker, so the trigger metric can swap without changing the loop shape.

The 14e default trigger threshold is `>= 0.7`. Tunable via `RebalancePolicy`.

## Three rebalancing approaches

### (A) Pure cron + standalone self-transfer

Background loop wakes on a fixed cadence. When the trigger fires, it builds and broadcasts a fresh transaction that consolidates or splits UTXOs to a target denomination distribution.

- **Pros:** Simple. Deterministic. Easy to reason about.
- **Cons:** Costs a full tx fee every time. Fires regardless of whether liquidity is actually being consumed right now. Wakes you up at 3am to merge dust even when nobody's trading. Adds chain bloat.

### (B) Pure on-the-fly during settlement

When coin selection picks N inputs for an RFQ settlement, opportunistically tack extra split/merge outputs onto the same transaction.

- **Pros:** Zero extra transaction fee — you're already paying for this tx. Happens exactly when liquidity is being consumed. No standalone rebalance txs at all.
- **Cons:** More complex tx construction (the rebalance plan has to mesh with the recipient's RGB invoice). Tx outputs leak some inventory shape via tx-structure analysis. Doesn't run when traffic is quiet, even if fragmentation is climbing.

### (C) Hybrid — periodic *planner*, settlement-tx *piggyback executor*

What this codebase implements (planner only in 14e; executor deferred).

A cron-ish loop monitors `fragmentation_score` / UTXO count / fee environment. When a trigger fires, it **publishes a `RebalancePlan`** — a list of proposed split / merge outputs. It does **not** broadcast anything. The next outgoing settlement transaction picks up the queued plan and bakes the extra outputs into its own output set alongside the recipient's RGB invoice fulfillment.

- **Pros:** Zero extra transaction fee (marginal byte cost only). Rebalance load self-throttles by traffic — a busy maker rebalances itself naturally. Monitoring and execution are cleanly separated. The planner is a pure function of inventory state; the executor is a property of the settlement-tx builder.
- **Cons:** If RFQ traffic dries up while fragmentation keeps climbing, (C) drifts.

## Why we picked (C)

Three reasons.

**Fees.** A standalone consolidation tx that merges 10 dust UTXOs into 1 medium UTXO is ~1500 vbytes. Piggybacking those same merges onto a settlement tx that was happening anyway adds ~150 vbytes (one output for the change-output redirection, plus accounting for the existing change output). Order of magnitude less fee paid by the maker.

**Self-throttling.** A busy maker doesn't need a separate rebalance schedule — every settlement is an opportunity. A quiet maker has nothing to rebalance, so doing nothing is the right move *most of the time*. (C) naturally tracks utilization.

**Architectural clarity.** Monitoring (`spawn_rebalance_loop` + `ExtendedInventorySnapshot` metrics) is pure; execution (splicing extra vouts into a PSBT) is a property of the settlement-tx builder. The two halves can be tested, deployed, and reasoned about independently. (A) and (B) couple them.

## The low-traffic caveat

(C) drifts if RFQ traffic dries up while fragmentation keeps climbing. Fallback design: after a configurable idle window (`REBALANCE_IDLE_FALLBACK_MS`, default e.g. 24 hours), if `fragmentation_score >= threshold` **and** no settlement has piggybacked the queued plan in that window, fire a standalone self-transfer — i.e., fall back to (A).

This fallback is **spec'd here but not implemented in 14e.** The trigger metrics exist; the idle-time tracking does not. Both land in the follow-up executor issue.

## What 14e ships vs what's deferred

**Shipped in 14e:**
- `fragmentation_score` and the related metrics (`average_input_count`, `average_change_ratio`, `pending_settlements`) in [ExtendedInventorySnapshot](../crates/rfq-types/src/lib.rs).
- `RebalancePolicy` configuration struct in [crates/maker-node/src/main.rs](../crates/maker-node/src/main.rs).
- `RebalancePlan` data type representing proposed merges / splits.
- `Maker::rebalance(policy) -> RebalancePlan` — pure planner, no broadcast.
- `spawn_rebalance_loop` running on `REBALANCE_INTERVAL_MS` (default 60s) that calls `rebalance()` and logs the plan when triggers fire.

**Deferred to follow-up issue (rebalance execution via settlement-tx piggybacking):**
- The actual splice that bakes `RebalancePlan` outputs into the next outgoing settlement PSBT.
- The idle-window fallback to (A) for quiet makers.
- Fee-environment-aware throttling (skip rebalance when fees are above N sat/vB).

The follow-up depends on [#13](broker-maker-node-roadmap.md)'s `create_transfer` being real (which is where the settlement PSBT is currently built).

## Related code references

- [ExtendedInventorySnapshot::from_utxos](../crates/rfq-types/src/lib.rs) — the metric computation
- [InventoryStore](../crates/rfq-store/src/lib.rs) — per-UTXO state the planner reads
- [Maker::rebalance](../crates/rfq-maker/src/lib.rs) — the planner itself
- [spawn_rebalance_loop](../crates/maker-node/src/main.rs) — the periodic trigger
- [broker-maker-node-roadmap.md](broker-maker-node-roadmap.md) — #14 issue + sub-PR breakdown
