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
- [ ] Implement CLI-backed `rfq-rgb` adapter
- [ ] Issue #9: Add RFQ settlement state machine
- [ ] Issue #6: Add OpenAPI spec for public RFQ API
- [ ] Issue #11: Explore splitting a maker allocation across multiple accepted buyers

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

## CLI-Backed `rfq-rgb` Adapter

Wrap the proven manual regtest RGB flow behind adapter traits.

- Add a command-backed RGB backend in `rfq-rgb`.
- Add maker-node config for RGB data dirs, contract id, Electrum URL, and wallet names.
- Keep normal tests mocked; add ignored integration tests for the Docker regtest stack.

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
- Whole-allocation reservation stays in place until partial allocation splitting is explored.
