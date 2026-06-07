# Consignment delivery & recovery — plan

Status: **planned (next session)**. Spans rgb-rfq (maker + broker) and colorex-wallet.

## Why

RGB consignments are ephemeral, and the **recipient must import (accept) one** to
see/spend received assets. Today this is fragile:

- The buy auto-imports `final_consignment` after `/sign` **best-effort** — if it
  fails (e.g. the witness tx isn't confirmed yet), the asset is **stranded**.
- **Nothing persists** the consignment: the dapp history saves only the witness
  txid; the maker builds the consignment at `/sign` and never stores it (maker.db
  has only `btc_utxos`/`rgb_utxos`).
- **No recovery path** if a consignment is lost (wallet reset, missed delivery,
  failed import). Observed live: swap `81060df…` broadcast 1 COLX to the taker
  seal, but the wallet never imported it → effectively lost to the taker.

Goal: a taker **never loses received RGB** to a timing/delivery failure, and we
can always re-deliver a consignment.

## Three components

### 1. Maker `reconsign` command (maker-node) — recovery primitive

Re-derive a consignment from the maker's stock (the stock still holds the
transfer transition).

- CLI: `colorex maker reconsign --contract <id> --outpoint <txid:vout> [--out <file>]`
  (witness txid inferred from the outpoint; `--out` writes base64, else stdout).
- Impl: reuse the `stock.transfer(contract_id, [output_seal], [], [], Some(witness_id))`
  path that `swap::commit_and_consign` already uses (witness-vout / output seal).
  Add a thin `RgbBackend::reconsign(contract, outpoint) -> consignment_b64`.
- Scope/limits: works for **witness-vout (explicit output) seals** — what swaps
  use. **Blinded/secret seals can't be re-derived** post-hoc (the outpoint is
  hidden); document that. Requires the witness tx be known to the resolver
  (ideally confirmed).
- Use: ops/support last-resort when the stash (below) doesn't have it.

### 2. Broker consignment stash service — client-facing recovery

Persist every settlement's consignment so clients can re-fetch it.

- **Persist on settle**: when the maker finalizes (`/sign`), store the
  `final_consignment` keyed by `quote_id` + `witness_txid` + recipient (invoice /
  seal). Lands on the broker (rfq-api) datastore — ties to **Postgres** (see
  [[project_broker_datastore]] / #30 v2). The maker can push it to the broker, or
  the broker captures it from the `/sign` response it already proxies.
- **Recovery endpoint**: `GET /consignments/{quote_id}` (and/or
  `GET /consignments?witness_txid=…`) → the stored consignment for re-import.
- **Auth/privacy**: a consignment reveals transfer detail — the endpoint must be
  scoped (caller proves ownership of the invoice/seal, e.g. a signature over the
  quote_id, or a per-quote recovery token issued at accept). Design before build.
- **Retention**: keep until N confirmations + a grace window; document policy.

### 3. Wallet import queue (colorex-wallet) — robust import

Make import survive confirmation timing and restarts.

- On receiving a consignment (swap auto-import OR manual paste), **enqueue it
  persistently** (IndexedDB) instead of a one-shot best-effort import.
- A **retry loop** runs `importAsset` until success: triggers on wallet open, a
  timer, and/or **witness-tx confirmation** (watch the txid via esplora — reuse
  the chain-sync path). Idempotent (re-importing an accepted consignment is a
  no-op).
- States per item: `pending → importing → done | failed(reason)`. Surface an
  "incoming RGB" badge + a retry/dismiss affordance in the wallet UI.
- Provider/UI: a **manual "Import consignment"** action (paste base64) →
  enqueue → `window.colorex.acceptConsignment` (already wired). Lets a user pull
  from the stash service or a maker `reconsign` output.

## End-to-end flow (all three)

1. Swap settles → dapp gets `final_consignment` → hands to wallet → wallet
   **enqueues** (non-blocking).
2. Wallet queue retries until the witness tx confirms + import succeeds → asset
   shows. Survives popup close / restart.
3. Lost it? Taker re-fetches from the **broker stash** (by quote_id) → re-enqueue.
4. Stash empty (old/evicted)? Maker **`reconsign`** re-derives from stock → hand
   to the taker → enqueue.

Also: dapp **persists the consignment in local history** + a "re-import" button,
so the common case never needs the stash.

## Build order (next session)

1. **Wallet import queue** — highest user value, self-contained; makes import
   robust immediately. Includes the manual-import UI + confirmation watch.
2. **Maker `reconsign`** — small recovery primitive; unblocks recovering the
   already-stranded `81060df…` COLX as the first test.
3. **Dapp**: enqueue-on-settle (replace best-effort import) + persist consignment
   in history + re-import affordance.
4. **Broker stash service** — persistence + authed recovery endpoint (with the
   Postgres datastore); largest, do last.

## Open questions

- Auth model for the stash recovery endpoint (ownership proof vs per-quote token).
- Confirmation-watch mechanism in the wallet (esplora polling vs a push).
- Stash retention + where exactly persistence lives (broker Postgres vs maker).
- Does RGB accept against a **mempool** (unconfirmed) witness tx, or is
  confirmation required? Determines whether the queue can import pre-confirmation.
  (Verify early — it shapes #1 and #2.)
