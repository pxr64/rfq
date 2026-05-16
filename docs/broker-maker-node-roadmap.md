# Broker + Maker-Node MVP Roadmap

This roadmap tracks the near-term MVP focus on broker and maker-node behavior before investing more in client app or browser wallet work.

The goal is to evolve from an in-process mock maker into a debuggable maker-node/broker system with clear inventory, cleanup, quote, and settlement lifecycle boundaries.

The MVP should work on Bitcoin regtest with a full RGB node and issued RGB20 assets. Core unit tests should remain mock-based, but the architecture must keep clear adapter boundaries for real regtest RGB integration.

## Roadmap

- [x] Issue #1: Add reservation-aware maker inventory
- [x] Issue #2: Add maker inventory snapshot endpoint/helper
- [x] Issue #3: Add quote expiry cleanup loop in maker-node
- [x] Issue #4: Wire maker-node runtime to broker/client surfaces
- [x] Define broker-to-maker node protocol
- [x] Define regtest RGB20 integration plan
- [x] Validate end-to-end NIA issuance + issuer→maker transfer on regtest
- [ ] Issue #13: Library-backed `rfq-rgb` adapter — foundation landed; real `LibRgbBackend` swap methods **parked**, issue needs a re-scope (see below)
- [x] Issue #14: RGB maker UTXO inventory management (supersedes #11)
- [x] Issue #15: Atomic swap settlement — buy side (BTC → RGB)
- [x] Issue #16: Atomic swap settlement — sell side (RGB → BTC)
- [x] Issue #9: Add RFQ settlement state machine
- [ ] Issue #6: Add OpenAPI spec for public RFQ API
- [ ] Future work: RGB ↔ RGB atomic swap (asset-for-asset). Two RGB state transitions committed in one Bitcoin tx; design pass deferred until #15/#16 land so we know the final `SwapLeg` / `RgbBackend` surface to extend.

## Issue #2: Inventory Snapshot

Add inventory observability so maker-node and broker health/debug flows can report state without the broker owning maker inventory.

- Add `InventorySnapshot` for amount totals and allocation counts.
- Track total, available, reserved, and spent inventory.
- Add `MockMaker::inventory_summary()` that releases expired reservations first.
- Test initial, reserved-after-quote, available-after-expiry, and spent-after-accept states.

## Issue #3: Expiry Cleanup Loop

Make maker-node release expired reservations without waiting for new RFQ traffic.

- Add a maker inventory cleanup method returning released reservation count.
- Add `CLEANUP_INTERVAL_MS` config to maker-node.
- Spawn a background cleanup loop in `maker-node run`.
- Add graceful shutdown with `tokio::signal::ctrl_c`.

## Issue #4: Maker-Node Runtime Wiring

Turn maker-node from a placeholder CLI into a useful daemon shell.

- Make `maker-node health` validate config, broker reachability, mock wallet, and inventory snapshot.
- Make `maker-node inventory` print `InventorySnapshot`.
- Keep `maker-node run` as a daemon shell with cleanup loop and placeholder quote serving until the broker-to-maker protocol exists.

## Broker-To-Maker Node Protocol

Define the network boundary between broker and external maker nodes.

- Define minimal HTTP maker-node API:
  - `GET /health`
  - `GET /inventory`
  - `POST /quotes`
  - `POST /quotes/{quote_id}/accept`
- Add an HTTP `MakerConnector` implementation while keeping in-process `MockMaker` for tests.
- Add integration coverage for broker talking to a test maker-node app.

## Regtest RGB20 Integration Plan

Define how the mock-only scaffolding becomes a real regtest MVP without leaking RGB dependencies into broker/core crates.

- Add Docker-based regtest infrastructure under `infra/regtest`.
- Use `bitcoind`, `electrs`, and pinned RGB sandbox-compatible CLI tooling.
- Document the manual NIA issuance and issuer-to-maker-to-taker transfer path.
- Keep real RGB node/client dependencies isolated in `rfq-rgb`.
- Keep wallet/key/PSBT/invoice behavior behind `rfq-wallet`.
- Add maker-node config for Bitcoin regtest, RGB node endpoint, RGB20 contract IDs, inventory, and wallet/key material.
- Keep `rfq-api`, `rfq-router`, `rfq-core`, and `rfq-types` free of real RGB implementation dependencies.
- Keep unit tests mock-based; add optional ignored integration tests for a running regtest Bitcoin/RGB environment later.

## Library-Backed `rfq-rgb` Adapter (Issue #13)

**Status: foundation + `create_invoice` landed; remaining real methods still parked pending a regtest-verifiable session.**

Wrap the proven manual regtest RGB flow behind the `RgbBackend` trait, using
`rgb-api` + `bp-std` + `bp-wallet` directly (the same libraries `rgb-cmd` builds
on). Subprocess shell-outs were considered and rejected: brittle text parsing,
no type safety, no isolation win since the RGB dep graph lives inside `rfq-rgb`
either way.

**Landed** (commits `664d51a` + #14a + this session):

- `LibRgbBackend` skeleton in `crates/rfq-rgb/src/lib_backend.rs`, with real
  `list_inventory_utxos` (reads the `Stock`) and `validate_invoice`.
- `RgbConfig` on `MakerNodeConfig` (env-driven: `RGB_NETWORK`, `ELECTRUM_URL`,
  `RGB_DATA_DIR`, `RGB_WALLET`, `RGB_CONTRACT_ID`); backend selection in
  `maker-node` + `rfq-api` (`Some(cfg)` → `LibRgbBackend`, `None` →
  `MockRgbBackend`).
- `rgb-*` / `bp-*` deps stay strictly inside `crates/rfq-rgb`; downstream
  callers import only the `RgbBackend` trait.
- Real `LibRgbBackend::load_wallet()` (Stock + `Wallet<XpubDerivable, RgbDescr>`
  via `bpwallet::fs::FsTextStore`) and `LibRgbBackend::resolver()` (Electrum-
  backed `AnyResolver`, network-checked) — the plumbing every remaining real
  method depends on.
- Real `LibRgbBackend::create_invoice`: coin-selects a keychain-9 outpoint,
  binds a fresh `GraphSeal`, stores the secret seal, emits an `RgbInvoice` via
  `RgbInvoiceBuilder`. Mirrors `rgb-cmd`'s `Invoice` command.

**Still parked** — `validate_incoming_consignment`, `create_swap_psbt_buy`,
`create_swap_psbt_sell`, `finalize_after_taker_sign` remain stubbed with
`TODO(#13)`. Reasons they still need a regtest-verifiable session:

- The original issue text predates the atomic-swap design (#15–#22): it
  describes `create_transfer` / `finalize_and_broadcast` / `list_allocations`,
  all since replaced or deleted. The trait the stubs must satisfy is the
  atomic-swap trait, not the unilateral-transfer one the issue assumes.
- The real work is atomic-swap PSBT construction: a *partial* PSBT the maker
  contributes only RGB inputs to, which the taker later funds with BTC inputs,
  with the witness txid deferred. `rgb-api` 0.11.1-rc.6 exposes only
  whole-transaction `wallet.pay()` / `construct_psbt()`; the two-party
  partial-PSBT shape needs custom assembly on top of those primitives.
- Acceptance is gated on live regtest Docker tests (`cargo test -p rfq-rgb --
  --ignored`), so the work needs an infra session to verify — it can't be
  signed off offline.

**Next step:** rewrite the #13 issue against the current atomic-swap
`RgbBackend` trait, then implement against a running regtest stack. Until then
the mock backend stack (`MockRgbBackend`) carries the buy/sell flows end-to-end.

## RGB Maker UTXO Inventory Management (Issue #14)

Track RGB-colored UTXOs as denomination units so the maker can fulfill RFQs safely at scale without UTXO bloat, fragmentation, or fee blow-up. Replaces the current whole-allocation reservation model. Supersedes #11 (allocation splitting becomes a side-effect of per-UTXO tracking).

Per-UTXO data model: `InventoryUtxo { outpoint, asset_id, amount, btc_sats, status, created_at, updated_at, pending_txid }` with an `InventoryStatus` enum covering `Available`, `Reserved { rfq_id, expires_at }`, `PendingBitcoinConfirm`, `PendingRgbAcceptance`, `Spent`, `Invalid`.

- Surface seal/outpoint per allocation in `rfq-rgb` (the data is already there in `LibRgbBackend`; just stop discarding it).
- Replace `MockMaker`'s whole-allocation reservation with per-UTXO reservation; atomic + concurrency-safe.
- Denomination-aware coin selection — start greedy-exact-fit (minimize input count, minimize change, prefer exact matches), iterate to fee-aware scoring later.
- Change management: re-ingest change UTXOs from successful transfers back into inventory; optionally normalize into target denomination buckets.
- Persistence behind an `rfq-store` adapter trait (SQLite or RocksDB): atomic reservation updates, durable settlement tracking, crash-safe recovery.
- Periodic rebalancing loop mirroring `spawn_cleanup_loop` in `crates/maker-node/src/main.rs` — trigger on UTXO count / fragmentation score / fee environment; maintain a target distribution policy.
- Extended inventory metrics: `total_balance`, `available_balance`, `reserved_balance`, `fragmentation_score`, `average_input_count`, `average_change_ratio`, `pending_settlements`.
- Failure handling: release stale reservations, reconcile chain state under reorgs, mark invalid allocations after failed broadcasts / failed RGB acceptance.
- Depends on **#13** for real per-UTXO data and the witness-tx round-trip; should land before **#9** (settlement state machine needs to know which UTXOs got reserved/spent).

### Sub-PR progress

Shipped as six sequenced sub-PRs (see `.claude/plans/yes-lets-do-ti-lovely-grove.md` for the full design).

- [x] **14a** — Surface per-UTXO outpoints in `rfq-rgb`. `Outpoint` + `RgbInventoryUtxo` in `rfq-types`; `RgbBackend::list_inventory_utxos`; `LibRgbBackend` stops discarding `seal.to_outpoint()`. (commit `2643517`)
- [x] **14b** — `InventoryStore` trait + `InMemoryInventoryStore`; remaining types (`InventoryStatus`, `InventoryUtxo`, `ReservationId`, `ExtendedInventorySnapshot`, `InventoryError`); legacy `InventorySnapshot` retained as `From<&ExtendedInventorySnapshot>` view.
- [x] **14c** — Per-UTXO reservation in `MockMaker` behind `InventoryStore`. `CoinSelector` trait + `WholeUtxoSelector` placeholder (14d swaps in `GreedyExactFitSelector`). Atomic reservation under contention verified by 10-task concurrent test. Legacy `inventory_snapshot` / `inventory_summary` preserved as derived views.
- [x] **14d** — `GreedyExactFitSelector` (exact-single → bounded 2-of-N exact up to N=2000 → bounded 3-of-N exact up to N=16 → smallest-change single → multi-UTXO greedy). Exclusion-based retry in `request_quote` so a fully deterministic selector still gets healthy concurrency. Fragmentation hot path verified: 100 dust UTXOs + 1 fat 10000 UTXO, request 20, picks `{10, 10}` not the fat one.
- [x] **14e** — `spawn_rebalance_loop` planner stub in maker-node + `RebalancePolicy` + `RebalancePlan`. Failure-handling `InventoryStore` methods (`mark_pending_bitcoin_confirm`, `mark_pending_rgb_acceptance`, `mark_broadcast_failed`, `mark_rgb_acceptance_failed`, `mark_reorged`, `mark_invalid`) + transition tests. Change re-ingestion in `MockMaker::accept_quote` when `expected_change > 0` (PendingBitcoinConfirm change UTXO). `docs/rebalancing-strategy.md` design doc.
- [x] **14f** — Deleted legacy `Allocation` / `AllocationState` / `ManagedAllocation` types and the `RgbBackend::list_allocations` trait method. `MockMaker::new` now takes `Vec<RgbInventoryUtxo>`; tests needing pre-Reserved / pre-Spent state use `with_components` + a hand-seeded `InMemoryInventoryStore`. `maker-node::build_maker` and `rfq-api::app` rewired to consume `list_inventory_utxos`. `LibRgbBackend::new` dropped its now-unused `maker_id` parameter.

### Follow-up issues

- **Rebalance executor** — splice `RebalancePlan` merges/splits into the next outgoing settlement PSBT. Depends on #13's real `LibRgbBackend` swap-PSBT construction. Includes the low-traffic fallback to standalone self-transfers (see `docs/rebalancing-strategy.md`).
- **Minimum liquidity floor** — explore how the maker guarantees at least N working-denomination UTXOs are always available so it can always serve a quote. The existing `min_utxo_count` rebalance trigger fires *after* the floor is breached but doesn't act. Design space: (a) hard floor on coin selection — refuse to fill quotes that would drop `available_utxos` below N, (b) proactive splits via the rebalance executor when approaching the floor, (c) reserved buffer subset that coin selection treats as untouchable. Likely a hybrid of (a) and (b). Should land alongside or after the rebalance executor.
- **Split `crates/maker-node/src/main.rs`** — the file is approaching 700 lines mixing config parsing, CLI, HTTP handlers, bootstrap, periodic loops, and tests. Break into focused modules: `config.rs` (`MakerNodeConfig`, `RgbConfig`, `RebalancePolicyConfig`), `cli.rs` (clap surface + subcommand routing), `http.rs` (`maker_app` + handlers), `bootstrap.rs` (`build_maker` + RGB backend wiring), `loops.rs` (`spawn_cleanup_loop`, `spawn_rebalance_loop`, `spawn_placeholder_loop`). Pure refactor, no behavioral change.

## BTC ↔ RGB Atomic Swap Settlement (Issues #15 + #16)

Replace the unilateral `MakerConnector::accept_quote` transfer with a real atomic swap where the maker assembles a witness transaction anchoring both BTC and RGB legs and broadcasts it after the taker returns a signed PSBT. Full protocol spec lives in [`docs/swap-flows.md`](swap-flows.md).

Two parent issues split by direction. Each parent has three lettered sub-issues following the #14 pattern.

- **#15** Buy side (taker buys RGB with BTC):
  - [x] **15a** (#17) — Side-aware protocol types (`SwapLeg`, expanded `SettlementIntent` / `SettlementStatus`, new `Quote` fee fields, two new `MakerConnector` methods, three new `ApiError` variants).
  - [x] **15b** (#18) — `BitcoinClient` trait + electrum-backed impl in a new `rfq-btc` crate (`get_outpoint`, `broadcast`, `estimate_feerate`, `block_height`).
  - [x] **15c** (#19) — Buy-side flow wiring (mock end-to-end): maker constructs PSBT with RGB inputs only, taker adds BTC at `/sign`, maker finalizes and broadcasts.
- **#16** Sell side (taker sells RGB for BTC):
  - [x] **16a** (#20) — `BtcInventoryStore` trait + `InMemoryBtcInventoryStore` (parallel to #14's RGB inventory, simpler shape).
  - [x] **16b** (#21) — Sell-side protocol additions (`SwapLeg::Sell` body, `Quote.maker_rgb_invoice`, new `POST /quotes/:id/consignment` endpoint, `SettlementStatus::AwaitingConsignment`).
  - [x] **16c** (#22) — Sell-side flow wiring (mock end-to-end): three round trips after accept (invoice → consignment → PSBT → sign → broadcast).

Buy and sell both settle end-to-end against the mock backend stack
(`MockMaker` + `MockRgbBackend` + `MockBitcoinClient`). The remaining gap is
the real RGB backend — `LibRgbBackend`'s swap methods are stubbed pending #13.

Protocol invariant:

> **Receiver of RGB assets creates the RGB invoice. Maker constructs the PSBT in both flows.**

Plus: **maker is the only broadcaster** (both flows), **fee policy is taker-pays** (`Quote.estimated_fee_sats` + `Quote.fee_slippage_bps`, default 20%), **segwit-only** (lets `expected_witness_txid` pre-compute once all inputs are committed).

Depends on **#14** (RGB inventory lifecycle is the same `Reserved → … → Spent` extended with `AwaitingTakerSignature` / `AwaitingConsignment` between accept and broadcast). Reuses existing `InventoryStore` failure-state methods (`mark_broadcast_failed`, `mark_reorged`, `mark_pending_bitcoin_confirm`, etc., from #14e). Touches **#9** — three new `SettlementStatus` variants. **#15 must land before #16** (sell reuses 15a's types and 15b's `BitcoinClient`).

## Issue #9: Settlement State Machine — done

Make the quote-acceptance / settlement lifecycle explicit. The issue's
originally-proposed state names (`PsbtCreated`, `Broadcasted`,
`ConsignmentSent`, `Validated`) predated the atomic-swap design; the machine
is built on the actual `SettlementStatus` variants instead.

Landed:

- `SettlementStatus` carries a documented transition graph (`Pending →
  Accepted/Awaiting* → AwaitingTakerSignature → PendingBitcoinConfirm →
  Settled`; any non-terminal → `Failed`).
- `SettlementStatus::{is_terminal, allowed_next, can_transition_to,
  transition}` in `rfq-types`; `transition` rejects disallowed steps with
  `SettlementTransitionError`.
- Tests cover both side-specific happy paths, stage skips / rewinds, self-
  transitions, and terminal dead-ends.

Follow-up (not #9): route the maker's per-quote status changes through
`transition()` at runtime — needs a persisted per-settlement status, which
belongs with settlement tracking / the `rfq-store` settlement records.

## Issue #6: OpenAPI Spec

Document the broker contract once the core broker and maker-node surfaces stabilize.

- Document broker API endpoints and schemas aligned with `rfq-types`.
- Include inventory/health endpoints only if the broker exposes them.

## Assumptions

- Near-term focus is broker + maker-node, not client app or browser wallet.
- Maker inventory remains maker-owned; the broker may observe inventory but should not reserve or mutate it directly.
- The MVP targets Bitcoin regtest with a full RGB node and issued RGB20 assets.
- Mocks remain the default for unit tests and local fast checks.
- Per-UTXO reservation + denomination-aware coin selection are the current MVP behavior (shipped under #14); whole-allocation reservation is gone.
- Receiver of RGB assets creates the RGB invoice in both directions; maker constructs the PSBT and broadcasts in both directions (see [`swap-flows.md`](swap-flows.md)).
- Fee policy is taker-pays. `Quote` carries `estimated_fee_sats` + `fee_slippage_bps`; settlement aborts if accept-time feerate exceeds the cap.
