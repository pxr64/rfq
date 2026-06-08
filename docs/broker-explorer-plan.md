# Broker settlements datastore & tx explorer — plan

Status: **planned**. Spans rgb-rfq (broker `rfq-api`, `rfq-store`, `rfq-btc`) and
colorex-dapp. Refs: **colorex-dapp#1** (explorer UI), **rgb-rfq#30** (broker
observability — shares the Postgres decision). The authed consignment stash is a
separate concern (component 4 / **rgb-rfq#33**), out of scope here.

## Why

- "Recent settlements" today is per-taker `localStorage` (`colorex-dapp
  src/history.ts`) — this device only.
- We want a **server-backed, browsable explorer** of the swaps the broker has
  relayed: a list + detail view, shareable, beyond one taker's local history.
- Persistence decision (see [[project_broker_datastore]]): the **broker uses
  Postgres**; the **maker keeps embedded SQLite**. Same `sqlx 0.8`, different driver.

## What the broker can record — and the confirmation gap

The broker proxies `/rfq → /accept → /consignment → /sign` between taker and maker
(`rfq-api/src/lib.rs`). At `/sign` it relays a `SettlementIntent` carrying
`quote_id`, `maker_id`, `status = PendingBitcoinConfirm`, `witness_txid` (and
`final_consignment`, which we do **not** store). The `Quote` it already holds
(`InMemoryQuoteStore`) adds the asset pair, amount, price, side, fee.

**The broker only witnesses settlement up to BROADCAST.** It never natively learns
the tx confirmed. On-chain "settled" status therefore comes from a **broker
background task polling electrum/electrs** via `rfq-btc` — the stack's single chain
dependency (the maker uses it already; only the *browser* wallet falls back to
esplora HTTP, because a browser can't open a TCP electrum socket).

## Design

### A. Datastore — `rfq-store`

- Add a **`postgres`** cargo feature: `sqlx` `PgPool` (`postgres` + `runtime-tokio`),
  mirroring the existing `sqlite` module/feature.
- New `SettlementRecord` + `SettlementStore` trait:
  - `save_settlement(record)` — upsert by `quote_id` (status advances
    Accepted → PendingBitcoinConfirm → Settled / Reorged / Failed).
  - `update_status(quote_id, status, confirmed_height?)` — the confirmation loop.
  - `list_settlements(filter, page)` — filters: `maker_id`, base/quote asset, side,
    status, `since_ms`; pagination (v1: `limit`/`offset`; can move to a
    `(created_at, quote_id)` cursor later).
  - `get_settlement(quote_id)`.
- Impls: `InMemorySettlementStore` (default; tests/CI build with no live PG) +
  `PostgresSettlementStore` (selected by `BROKER_DATABASE_URL`).
- **`SettlementRecord` fields (metadata only — privacy):** `quote_id`, `maker_id`,
  `base_asset`, `quote_asset`, `side`, `amount`, `price`, `fee_sats`,
  `witness_txid` (nullable until `/sign`), `status`, `confirmed_height` (nullable),
  `created_at_ms`, `updated_at_ms`. **No consignment blob, no taker identifiers.**
- Schema: `settlements` table, PK `quote_id`, indexes on `status`, `maker_id`,
  `witness_txid`, `created_at_ms`.

### B. Broker recording — `rfq-api`

- `AppState` gains `settlement_store: Arc<dyn SettlementStore>`.
- Record **best-effort** along the lifecycle (a store failure logs, never breaks the
  relayed settlement):
  - `/accept` → upsert row, status `Accepted` + quote metadata.
  - `/sign` → update status `PendingBitcoinConfirm` + `witness_txid`.
- Store selected by `BROKER_DATABASE_URL` (Postgres) else in-memory. Config lives
  alongside `BROKER_LISTEN` in `main.rs`; add `BROKER_ELECTRUM_URL` for the loop.

### C. Confirmation loop — `rfq-btc` + `rfq-api`

- Add to `rfq-btc::BitcoinClient`: `tx_status(txid) -> TxConfirmation { confirmed,
  height: Option<u32> }` over electrum (`transaction.get` + merkle / verbose). The
  `MockBitcoinClient` returns a configurable status so the loop is testable without
  a live electrs.
- A broker **background tokio task** (sibling to the maker's chain-observer/cleanup
  loops — a "cron" thread, not OS cron), spawned at serve time: every poll interval,
  list settlements in `PendingBitcoinConfirm`, query `tx_status` per distinct
  `witness_txid`, and promote to `Settled` (+ height) once it has `≥ N` confirmations.
- Knobs: `BROKER_CONFIRMATIONS` (default 1), poll interval. v1 only promotes to
  Settled; richer reorg/failed detection over electrum is a follow-up.

### D. Explorer endpoint — `rfq-api`

- `GET /settlements?maker=&base=&quote=&side=&status=&since=&limit=&offset=` →
  `{ items: [public row], next? }`.
- Public row: `quote_id`, `maker_id`, `pair` (base→quote), `amount`, `price`,
  `side`, `witness_txid`, `status`, `confirmed_height?`, `created_at_ms`.
- Optional `GET /settlements/:quote_id` detail (same public fields).
- No auth for v1 (public, metadata-only). The record simply has no taker/consignment
  fields to leak.

### E. Dapp explorer page — colorex-dapp (immediately-following pass)

- Broker client method `getSettlements(filters)` (`adapters/broker/client.ts`).
- New **Explorer** route/view: list (pair, amount, maker, status pill, age,
  `witness_txid` → mempool link via `explorer.ts`) + filters; a detail panel. Reuse
  the existing design tokens. Add a nav entry.
- The local-history "Recent settlements" rail stays as the fast, offline "my swaps"
  view; the explorer is the broader, server-backed one.

## Build order

1. **`rfq-store`**: `postgres` feature + `SettlementStore` (trait + InMemory +
   Postgres) + tests (in-memory always; PG behind the feature/env).
2. **`rfq-btc`**: `tx_status` on `BitcoinClient` (+ mock).
3. **`rfq-api`**: `AppState` wiring + record at `/accept` + `/sign` + the
   confirmation loop + `GET /settlements` + config
   (`BROKER_DATABASE_URL`, `BROKER_ELECTRUM_URL`, `BROKER_CONFIRMATIONS`).
4. **(next pass) colorex-dapp**: the explorer view.

## Decisions (locked)

- **Confirmation** via a broker background **electrum/electrs** poll (the only chain
  dependency); add `tx_status` to `rfq-btc`, reusable by the maker.
- **Backend-first**; the dapp explorer UI follows once the endpoint shape is real.
- **Public, metadata-only** rows: no taker identity, no consignment blob (the witness
  txid is already public on-chain).
- **Postgres** for the broker (in-memory fallback for dev/CI); the maker stays SQLite.

## Open / later

- Reorg / failed-status fidelity over electrum (v1: promote-to-settled only).
- Cursor pagination (v1 can be `limit`/`offset`).
- Maker-authed detail view (fees, consignment ref) — later.
- Confirmation could later be **pushed by makers** over the rgb-rfq#30 WS
  (`Heartbeat`/`InventoryUpdate` siblings) instead of polled — fewer electrum calls.
- Dev Postgres: a docker-compose `postgres` service + a migrations strategy
  (`sqlx migrate` vs the `CREATE TABLE IF NOT EXISTS` approach `sqlite.rs` uses).
