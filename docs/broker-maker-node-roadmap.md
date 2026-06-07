# Broker + Maker-Node MVP Roadmap

This roadmap tracks the near-term MVP focus on broker and maker-node behavior before investing more in client app or browser wallet work.

The goal is to evolve from an in-process mock maker into a debuggable maker-node/broker system with clear inventory, cleanup, quote, and settlement lifecycle boundaries.

The MVP should work on Bitcoin regtest with a full RGB node and issued RGB20 assets. Core unit tests should remain mock-based, but the architecture must keep clear adapter boundaries for real regtest RGB integration.

## Roadmap

- [ ] **TOP PRIORITY — Issue #32: Inventory-reservation griefing.** A taker can lock
  a maker's inventory without ever settling. Today the maker reserves RGB at
  `request_quote` (`QUOTE_TTL_MS = 30s`) and `accept` extends to
  `TAKER_SIGNATURE_TTL_MS = 10min`, so (a) quote-spam reserves inventory for free and
  (b) accept-and-abandon denies service for 10 min (hit live: an aborted buy left the
  only COLX UTXO `Reserved`, nothing broadcast). Fix order: **(1) don't reserve on
  quote** (price from the order book; just check it could fill); **(2) reserve only
  after the funds/build check** in `accept`; **(3) short accept→sign TTL (~60–120s) +
  `POST /quotes/{id}/cancel`** the dapp calls on user-cancel/sign-failure; **(4)
  per-origin reservation caps + RFQ rate-limit**; (5, later) fidelity bond. See the
  issue for acceptance criteria.
- [x] **Switch the swap close method from opret to tapret.** (Shipped.) Today
  the swap composition is hard-wired to `CloseMethod::OpretFirst` (an `OP_RETURN`
  output carries the RGB commitment), which flags every swap as an RGB tx on-chain.
  Tapret tweaks the commitment into a Taproot output instead, so a swap is
  indistinguishable from an ordinary P2TR spend — essential before going to a
  public network (signet/mainnet). Scope: (a) switch maker/taker/issuer wallets
  from `wpkh` to `tr(...)` descriptors (re-create wallets, re-issue the contract,
  re-bootstrap — genesis is descriptor-bound); (b) replace the opret host
  (`set_rgb_close_method(OpretFirst)` + the 0-value `OP_RETURN` host output) with
  the tapret host on a maker-controlled Taproot output (typically its change);
  (c) Schnorr/BIP340 key-path signing for the maker + taker inputs (partial
  `tap_*` plumbing already exists in `enrich_psbt_input`); (d) RGB seal anchors
  move keychain 9 → 10 (`RgbKeychain::Tapret`). ORTHOGONAL to the witness-vout /
  blinded-seal work — those are about *where RGB lands*, tapret is about *where the
  commitment is anchored*; the seal logic carries over unchanged.
- [x] Issue #1: Add reservation-aware maker inventory
- [x] Issue #2: Add maker inventory snapshot endpoint/helper
- [x] Issue #3: Add quote expiry cleanup loop in maker-node
- [x] Issue #4: Wire maker-node runtime to broker/client surfaces
- [x] Define broker-to-maker node protocol
- [x] Define regtest RGB20 integration plan
- [x] Validate end-to-end NIA issuance + issuer→maker transfer on regtest
- [x] Issue #13: Library-backed `rfq-rgb` adapter — full swap-PSBT trio (B1–B5), validate semantics + trait-sig fix, maker-node electrum wiring, seal-anchor BTC routing (#25) all shipped. Closed.
- [x] Issue #14: RGB maker UTXO inventory management (supersedes #11)
- [x] Issue #15: Atomic swap settlement — buy side (BTC → RGB)
- [x] Issue #16: Atomic swap settlement — sell side (RGB → BTC)
- [x] Issue #9: Add RFQ settlement state machine
- [x] Issue #23: Self-contained Rust e2e tests for rfq-rgb — in-Rust bootstrap landed; ephemeral docker stack split out as #26.
- [x] Issue #25: Swap composition: route seal-anchor BTC value to a maker change output (don't burn as fee).
- [ ] Issue #27: Maker-node runtime — post-broadcast wallet refresh + BTC inventory refresh + confirmation tracking. **Last critical gap before the daemon is fully operational on regtest** (today's state stops working after the first swap broadcasts).
- [ ] Issue #29: Multi-asset maker — the daemon wires a single `[rgb] contract_id` end-to-end, so a maker is effectively single-asset even though the order book is already `(asset, side)`-keyed. Lift `build_runtime` to an active *set* of contracts (per-asset RGB + BTC inventory + chain observer) and add `--asset` to `maker invoice`/`inventory`.
- [x] Maker→broker auto-discovery over WebSocket — makers dial the broker and self-register (`/maker-stream` + `MakerRegistry` + `WsMakerConnector`). The static `BROKER_MAKER` env pre-seed has been removed: the broker registry always starts empty and is filled by self-registration.
- [~] Issue #30: Broker observability. **v1 landed**: `GET /status` reports `makers_online`, `asset_pairs`, `networks`, and per-maker `uptime_secs` (registry gained `connected_at`; the `Register` frame now carries `network` + served `assets`). **Deferred (v2)**: median quote latency, quotes-routed-24h, settlement-success, online-vs-subscribed — need timestamped event capture + persistence (heartbeat/`InventoryUpdate` frames).
- [ ] Issue #24: Replace bp-hot + easy rgb-cmd commands in the e2e test harness — bp-hot slice landed; rgb-cmd commands (import/create/address/utxos/invoice/transfer/accept) still subprocess.
- [ ] Issue #26: Run regtest stack ephemerally via testcontainers-rs so `cargo test` owns its bitcoind + electrs lifecycle.
- [x] Issue #6: OpenAPI spec for the broker API — generated in-code with `utoipa` (NestJS-style): `#[utoipa::path]` on the handlers + `ToSchema` on `rfq-types`. The broker serves Swagger UI at `GET /swagger-ui` and the spec at `GET /api-docs/openapi.json`, always in sync with the code. Covers `/health`, `/status`, `/rfq`, and the `accept`/`consignment`/`sign` quote routes; `/maker-stream` is intentionally excluded. TS clients can be generated from the served `openapi.json`.
- [ ] Future work: RGB ↔ RGB atomic swap (asset-for-asset). Two RGB state transitions committed in one Bitcoin tx; design pass deferred until #15/#16 land so we know the final `SwapLeg` / `RgbBackend` surface to extend.

## Issue #2: Inventory Snapshot

Add inventory observability so maker-node and broker health/debug flows can report state without the broker owning maker inventory.

- Add `InventorySnapshot` for amount totals and allocation counts.
- Track total, available, reserved, and spent inventory.
- Add `Maker::inventory_summary()` that releases expired reservations first.
- Test initial, reserved-after-quote, available-after-expiry, and spent-after-accept states.

## Issue #3: Expiry Cleanup Loop

Make maker-node release expired reservations without waiting for new RFQ traffic.

- Add a maker inventory cleanup method returning released reservation count.
- Add `intervals.cleanup` config (formerly `CLEANUP_INTERVAL_MS` env var) to maker-node.
- Spawn a background cleanup loop in `colorex maker up`.
- Add graceful shutdown with `tokio::signal::ctrl_c`.

## Issue #4: Maker-Node Runtime Wiring

Turn maker-node from a placeholder CLI into a useful daemon shell.

- Make `colorex maker health` validate config, broker reachability, mock wallet, and inventory snapshot.
- Make `colorex maker inventory` print `InventorySnapshot`.
- Keep `colorex maker up` as a daemon shell with cleanup loop and placeholder quote serving until the broker-to-maker protocol exists.

## Broker-To-Maker Node Protocol

Define the network boundary between broker and external maker nodes.

- Define minimal HTTP maker-node API:
  - `GET /health`
  - `GET /inventory`
  - `POST /quotes`
  - `POST /quotes/{quote_id}/accept`
- Add an HTTP `MakerConnector` implementation while keeping in-process `Maker` for tests.
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

**Status: done.** Foundation + `create_invoice` + `validate_incoming_consignment` + the full swap-PSBT trio (B1 plumbing, B2 buy composition, B3 sell composition, B4 finalize-after-taker-sign, B5 two-backend round-trip tests) all landed and regtest-verified. Both buy and sell round-trips broadcast through bitcoind end-to-end with cooperating maker + taker `LibRgbBackend`s.

Wrap the proven manual regtest RGB flow behind the `RgbBackend` trait, using
`rgb-api` + `bp-std` + `bp-wallet` directly (the same libraries `rgb-cmd` builds
on). Subprocess shell-outs were considered and rejected: brittle text parsing,
no type safety, no isolation win since the RGB dep graph lives inside `rfq-rgb`
either way.

**Landed** (commits `664d51a` + #14a + this session):

- `LibRgbBackend` skeleton in `crates/rfq-rgb/src/lib_backend.rs`, with real
  `list_inventory_utxos` (reads the `Stock`) and `validate_invoice`.
- `RgbConfig` on `MakerNodeConfig` (TOML-driven `[rgb]` table with `network`,
  `electrum_url`, `data_dir`, `wallet_name`, `contract_id`); backend selection in
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

- Real `LibRgbBackend::validate_incoming_consignment`: base64-decode →
  `Transfer::load` → contract-id cross-check → `validate(&resolver, &config)`
  against electrs + the maker's trusted typesystem → introspect
  `bundled_witnesses` for `(outpoint, amount)` tuples into `ConsignmentInfo`.
  Mirrors `rgb-cmd`'s `Validate`/`Accept`. Live-verified by
  `tests/cli.rs::validate_incoming_consignment_accepts_a_real_consignment`.

**Group B — composition** delivered the two-party swap-PSBT trio. Unlike the
methods above, these had no `rgb-cmd` analog to mirror — rgb-cmd only does
unilateral transfers. Every primitive was already public (bp-std `Psbt`,
`psbt.sign(&signer)`, rgb-api's `stock.transition_builder_raw`,
`psbt.rgb_embed`/`rgb_commit`, `stock.consume_fascia`/`transfer`); the
work was **composing them below `pay()`** into the two-party shape:
partial PSBT, taker contributes BTC for buy / RGB for sell, per-input
SIGHASH, stable witness txid via declared-funding.

**Design**: [`docs/swap-psbt-design.md`](swap-psbt-design.md) — the
implementation-level companion to `docs/swap-flows.md`. Defines the
inputs/outputs/signing matrix per side, the lifecycle per trait method,
six explicit design decisions, the known unknowns each phase resolved,
and the five-phase plan (B1 plumbing → B2 buy → B3 sell → B4 finalize →
B5 tests).

**Phase commits:**

- B1 plumbing: `f865ce4`, `66b3e78`
- B2 buy composition: `c803a0d`, `163d776`
- B3 sell composition: `efc1293` + correction `0987676`
- B4 finalize: `98addd3` (+ prereq `70fb2a2` promoting
  `enrich_psbt_input` to pub)
- B5 round-trip tests: `a6f8d81` — two-backend buy + sell, broadcasts
  through bitcoind

### Resolved follow-ups (post-Group B)

All Group B follow-ups that surfaced during B3–B5 have shipped:

- ✅ **`validate_incoming_consignment` semantics + trait-sig cleanup**
  (`a6227b8`) — now returns *input* outpoints from terminal transitions
  via a new `extract_input_outpoints` helper; trait sig takes typed
  `expected_contract_id: ContractId` (rfq-maker threads it from
  `parse_maker_invoice`). B5 sell test's pre-consign inventory
  workaround removed.
- ✅ **Maker-node electrum + BTC wallet bootstrap** (`5542da5`) —
  `ElectrumClient::get_outpoint` wired through (uses electrum-client's
  vendored `bitcoin` crate to parse the witness tx). New
  `LibRgbBackend::list_btc_only_utxos` returns wallet UTXOs minus the
  RGB-bearing ones. `build_maker` branches on `config.rgb`: Some →
  real `ElectrumClient` + real BTC inventory; None → keeps mock for
  tests + demo.
- ✅ **Seal-anchor BTC value handling** (`fee3c6a`, closes #25) — buy
  adds a `maker_btc_change` output for the consumed seal-anchor sats;
  sell folds the taker's RGB-input BTC value into the taker payout.
  Tests broadcast cleanly without `maxfeerate=0`.

The remaining maker-node operational gap (post-broadcast wallet
refresh + BTC inventory refresh + confirmation tracking) is tracked
separately as #27.

## RGB Maker UTXO Inventory Management (Issue #14)

Track RGB-colored UTXOs as denomination units so the maker can fulfill RFQs safely at scale without UTXO bloat, fragmentation, or fee blow-up. Replaces the current whole-allocation reservation model. Supersedes #11 (allocation splitting becomes a side-effect of per-UTXO tracking).

Per-UTXO data model: `InventoryUtxo { outpoint, asset_id, amount, btc_sats, status, created_at, updated_at, pending_txid }` with an `InventoryStatus` enum covering `Available`, `Reserved { rfq_id, expires_at }`, `PendingBitcoinConfirm`, `PendingRgbAcceptance`, `Spent`, `Invalid`.

- Surface seal/outpoint per allocation in `rfq-rgb` (the data is already there in `LibRgbBackend`; just stop discarding it).
- Replace `Maker`'s whole-allocation reservation with per-UTXO reservation; atomic + concurrency-safe.
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
- [x] **14c** — Per-UTXO reservation in `Maker` behind `InventoryStore`. `CoinSelector` trait + `WholeUtxoSelector` placeholder (14d swaps in `GreedyExactFitSelector`). Atomic reservation under contention verified by 10-task concurrent test. Legacy `inventory_snapshot` / `inventory_summary` preserved as derived views.
- [x] **14d** — `GreedyExactFitSelector` (exact-single → bounded 2-of-N exact up to N=2000 → bounded 3-of-N exact up to N=16 → smallest-change single → multi-UTXO greedy). Exclusion-based retry in `request_quote` so a fully deterministic selector still gets healthy concurrency. Fragmentation hot path verified: 100 dust UTXOs + 1 fat 10000 UTXO, request 20, picks `{10, 10}` not the fat one.
- [x] **14e** — `spawn_rebalance_loop` planner stub in maker-node + `RebalancePolicy` + `RebalancePlan`. Failure-handling `InventoryStore` methods (`mark_pending_bitcoin_confirm`, `mark_pending_rgb_acceptance`, `mark_broadcast_failed`, `mark_rgb_acceptance_failed`, `mark_reorged`, `mark_invalid`) + transition tests. Change re-ingestion in `Maker::accept_quote` when `expected_change > 0` (PendingBitcoinConfirm change UTXO). `docs/rebalancing-strategy.md` design doc.
- [x] **14f** — Deleted legacy `Allocation` / `AllocationState` / `ManagedAllocation` types and the `RgbBackend::list_allocations` trait method. `Maker::new` now takes `Vec<RgbInventoryUtxo>`; tests needing pre-Reserved / pre-Spent state use `with_components` + a hand-seeded `InMemoryInventoryStore`. `maker-node::build_maker` and `rfq-api::app` rewired to consume `list_inventory_utxos`. `LibRgbBackend::new` dropped its now-unused `maker_id` parameter.

### Follow-up issues

- **Rebalance executor** — splice `RebalancePlan` merges/splits into the next outgoing settlement PSBT. Depends on #13's real `LibRgbBackend` swap-PSBT construction. Includes the low-traffic fallback to standalone self-transfers (see `docs/rebalancing-strategy.md`).
- **Minimum liquidity floor** — explore how the maker guarantees at least N working-denomination UTXOs are always available so it can always serve a quote. The existing `min_utxo_count` rebalance trigger fires *after* the floor is breached but doesn't act. Design space: (a) hard floor on coin selection — refuse to fill quotes that would drop `available_utxos` below N, (b) proactive splits via the rebalance executor when approaching the floor, (c) reserved buffer subset that coin selection treats as untouchable. Likely a hybrid of (a) and (b). Should land alongside or after the rebalance executor.
- **Split `crates/maker-node/src/main.rs`** — the file is approaching 700 lines mixing config parsing, CLI, HTTP handlers, bootstrap, periodic loops, and tests. Break into focused modules: `config.rs` (`MakerNodeConfig`, `RgbConfig`, `RebalancePolicyConfig`), `cli.rs` (clap surface + subcommand routing), `http.rs` (`maker_app` + handlers), `bootstrap.rs` (`build_maker` + RGB backend wiring), `loops.rs` (`spawn_cleanup_loop`, `spawn_rebalance_loop`, `spawn_placeholder_loop`). Pure refactor, no behavioral change.
- **Wallet UTXO-cache sync** — `LibRgbBackend::load_wallet` only deserializes the on-disk bp-wallet cache (`Wallet::load` does no network I/O), and we deliberately do *not* sync inside the backend. `list_inventory_utxos` and `create_swap_psbt_*` (via `resolve_maker_inputs`) therefore read whatever was last persisted. In regtest the bootstrap scripts scan + persist, so the cache is current; a long-running maker-node has no such refresh. The maker's own swap tx spends an RGB input + creates change, and sell-side receives / BTC deposits arrive out-of-band — all stale the cache. Wire an incremental `wallet.wallet_mut().update(&indexer)` (bp-electrum `AnyIndexer`, *not* a full `sync_from_scratch`) into the maker-node lifecycle: once at startup before seeding inventory, and after each settlement confirms (the cleanup/confirm loop already polls block height). Keep it out of the per-call backend path (latency + layering). Note the rfq-maker `InventoryStore` already tracks reservation/spent/pending-confirm as the operational availability truth; the wallet cache only needs freshness for per-input value + derivation terminal at PSBT-build time. An earlier `sync_wallet` helper (removed in B2b) is the starting point.

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
(`Maker` + `MockRgbBackend` + `MockBitcoinClient`). The remaining gap is
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

## Issue #29: Multi-Asset Maker

A maker is **single-asset** today, even though the pricing layer is multi-asset.
The order book keys orders by `(asset_id, side)` (`crates/maker-node/src/orders.rs:71`)
and `order create --asset` accepts any contract — but the running daemon only
wires the one `[rgb] contract_id` end-to-end, so a second asset can be *priced*
yet never *settled*.

Single-asset wiring lives in `build_runtime` (`crates/maker-node/src/lib.rs:316`):
it derives one `asset` from `contract_id`, loads RGB inventory only for it
(`list_inventory_utxos(&asset)`, `lib.rs:344`), and binds BTC inventory + the
chain observer to that single asset (`lib.rs:356`, `846`/`864`). `maker invoice`
/ `maker inventory` use the config `contract_id` (and `invoice` errors if it's
empty, `lib.rs:293`). The RGB stash itself is already multi-contract — the limit
is purely daemon wiring.

Scope:

- Config: an active *set* of contracts (a `contract_ids` list, or derive from the
  order book / stock), back-compatible with the single `contract_id` field.
- `build_runtime`: per-asset RGB + BTC inventory; `Maker` holds inventory keyed by
  asset.
- Chain observer: track all active assets (per-asset / `Vec<asset>`).
- CLI: `--asset` on `maker invoice` and `maker inventory`.
- Verify swap-PSBT construction resolves the correct contract's allocations from
  the multi-asset inventory (the quote path already keys on the RFQ asset via
  `PricePolicy`).

Relates to #27 (per-asset cache freshness multiplies the refresh surface) and
builds on the per-UTXO inventory work (#14).

## Issue #30: Broker Observability

The maker→broker WebSocket auto-discovery (the `MakerRegistry` + `/maker-stream`
+ `WsMakerConnector`) means the broker now *knows* which makers are connected —
the foundation for operational awareness it currently lacks.

Scope:

- WS protocol (`rfq-router::ws_protocol`): maker→broker `Heartbeat` +
  `InventoryUpdate` (push an `InventorySnapshot` per `(asset, side)` periodically
  / on change) over the existing duplex socket.
- `MakerRegistry` per-maker metadata: `connected_at` (uptime), `last_seen`
  (heartbeat-driven liveness), latest inventory snapshot.
- `GET /status` (and/or `/makers`): `makers_online`, per-maker
  `{uptime, last_seen}`, and aggregate available liquidity per `(asset, side)`
  summed across makers. Prometheus `/metrics` later.

Builds on the WS auto-discovery feature; reuses `rfq_types::InventorySnapshot`
(already produced by `Maker::inventory_summary`).

## Issue #6: OpenAPI Spec — DONE

The broker contract is generated in-code with `utoipa` and served by the broker
itself (NestJS-style), so it never drifts from the implementation:

- **Swagger UI:** `GET /swagger-ui` — interactive docs + "Try it out".
- **Raw spec:** `GET /api-docs/openapi.json`.
- Source of truth: `#[utoipa::path]` annotations on the `rfq-api` handlers and
  `#[derive(ToSchema)]` on the `rfq-types` wire structs (`ApiDoc` in
  `crates/rfq-api/src/lib.rs`).
- All broker routes: `/health`, `/status`, `/rfq`, `/quotes/{id}/accept`,
  `/quotes/{id}/consignment`, `/quotes/{id}/sign`. Schemas: `CreateRfqRequest`,
  `Quote`, `SwapLeg` (`side` discriminator), `SettlementIntent`/
  `SettlementStatus`, `StatusResponse`, uniform `ErrorResponse`, etc.
- The `/maker-stream` WebSocket is intentionally out of scope (maker↔broker
  transport, not the public taker API).

A TS client can be generated from the served spec, e.g.
`openapi-typescript http://127.0.0.1:3000/api-docs/openapi.json`.

## Assumptions

- Near-term focus is broker + maker-node, not client app or browser wallet.
- Maker inventory remains maker-owned; the broker may observe inventory but should not reserve or mutate it directly.
- The MVP targets Bitcoin regtest with a full RGB node and issued RGB20 assets.
- Mocks remain the default for unit tests and local fast checks.
- Per-UTXO reservation + denomination-aware coin selection are the current MVP behavior (shipped under #14); whole-allocation reservation is gone.
- Receiver of RGB assets creates the RGB invoice in both directions; maker constructs the PSBT and broadcasts in both directions (see [`swap-flows.md`](swap-flows.md)).
- Fee policy is taker-pays. `Quote` carries `estimated_fee_sats` + `fee_slippage_bps`; settlement aborts if accept-time feerate exceeds the cap.
