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
- [ ] Issue #13: Implement library-backed `rfq-rgb` adapter
- [ ] Issue #14: RGB maker UTXO inventory management (supersedes #11)
- [ ] Issue #9: Add RFQ settlement state machine
- [ ] Issue #6: Add OpenAPI spec for public RFQ API

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

Wrap the proven manual regtest RGB flow behind adapter traits, using `rgb-std` + `bp-wallet` crates directly (the same libraries `rgb-cmd` builds on). Subprocess shell-outs were considered and rejected: brittle text parsing, no type safety, no win on isolation since the RGB dep graph still lives inside `rfq-rgb` either way.

- Add `LibRgbBackend` in `rfq-rgb` using `Stock`, `RgbWallet`, `RgbInvoice`, `ContractBuilder` directly.
- Expand the `RgbBackend` trait with `finalize_and_broadcast` so signing stays outside rfq-rgb (lives in rfq-wallet eventually).
- Add maker-node config for RGB data dir, contract id, Electrum URL, wallet name, network.
- Keep `rgb-*` / `bp-*` deps strictly inside `crates/rfq-rgb`; downstream callers only import the rfq-rgb trait.
- Keep normal tests mocked; add `#[ignore]` integration tests under `crates/rfq-rgb/tests/cli.rs` for the Docker regtest stack.

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

Shipped as five sequenced sub-PRs (see `.claude/plans/yes-lets-do-ti-lovely-grove.md` for the full design).

- [x] **14a** — Surface per-UTXO outpoints in `rfq-rgb`. `Outpoint` + `RgbInventoryUtxo` in `rfq-types`; `RgbBackend::list_inventory_utxos`; `LibRgbBackend` stops discarding `seal.to_outpoint()`. (commit `2643517`)
- [x] **14b** — `InventoryStore` trait + `InMemoryInventoryStore`; remaining types (`InventoryStatus`, `InventoryUtxo`, `ReservationId`, `ExtendedInventorySnapshot`, `InventoryError`); legacy `InventorySnapshot` retained as `From<&ExtendedInventorySnapshot>` view.
- [x] **14c** — Per-UTXO reservation in `MockMaker` behind `InventoryStore`. `CoinSelector` trait + `WholeUtxoSelector` placeholder (14d swaps in `GreedyExactFitSelector`). Atomic reservation under contention verified by 10-task concurrent test. Legacy `inventory_snapshot` / `inventory_summary` preserved as derived views.
- [ ] **14d** — `GreedyExactFitSelector` (exact-match → bounded subset-sum → smallest-change vs smallest-input-count tie-break).
- [ ] **14e** — Rebalance loop stub, extended metrics, failure handling, change re-ingestion; `docs/rebalancing-strategy.md`; deletion of legacy `Allocation` / `AllocationState` / `ManagedAllocation`.

## Issue #9: Settlement State Machine

Make quote acceptance and settlement lifecycle explicit before real RGB/Bitcoin execution.

- Add explicit settlement states and transition validation.
- Keep Bitcoin/RGB execution mocked.
- Reject invalid transitions in tests.

## Issue #6: OpenAPI Spec

Document the broker contract once the core broker and maker-node surfaces stabilize.

- Document broker API endpoints and schemas aligned with `rfq-types`.
- Include inventory/health endpoints only if the broker exposes them.

## Assumptions

- Near-term focus is broker + maker-node, not client app or browser wallet.
- Maker inventory remains maker-owned; the broker may observe inventory but should not reserve or mutate it directly.
- The MVP targets Bitcoin regtest with a full RGB node and issued RGB20 assets.
- Mocks remain the default for unit tests and local fast checks.
- Whole-allocation reservation is the current MVP behavior; per-UTXO reservation + denomination-aware coin selection arrive under issue #14.
