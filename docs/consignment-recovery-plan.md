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

### 1. Maker consignment persistence + `reconsign` (maker-node)

The maker should **hold its own consignments** (not rely solely on the broker),
with `reconsign` as the re-derive fallback.

- **Persist on produce**: store every `final_consignment` the maker generates in a
  new `maker.db` table (`consignments`: `quote_id`, `witness_txid`, `contract_id`,
  recipient seal, blob, `created_at`). The maker can then re-serve a consignment
  directly — cheap, no re-derive, no chain needed.
- **`reconsign` (fallback)**: `colorex maker reconsign --contract <id> --outpoint <txid:vout> [--out <file>]`
  re-derives from the stock via the `stock.transfer(contract_id, [output_seal], [],
  [], Some(witness_id))` path `swap::commit_and_consign` already uses. Add a thin
  `RgbBackend::reconsign(contract, outpoint) -> consignment_b64`. Works for
  **witness-vout (explicit output) seals** — what swaps use; **blinded/secret
  seals can't be re-derived** post-hoc (outpoint hidden) → document. This covers
  consignments produced before persistence existed (e.g. the stranded `81060df…`).
- Redundancy: **maker holds its own**, **broker holds all** (next). Either can
  serve a recovery.

### 2. Broker consignment stash service — client-facing recovery

Persist every settlement's consignment **across all makers** so any client can
re-fetch it (complements the maker holding its own).

- **Persist on settle**: when the maker finalizes (`/sign`), store the
  `final_consignment` keyed by `quote_id` + `witness_txid` + recipient (invoice /
  seal). Lands on the broker (rfq-api) datastore — ties to **Postgres** (see
  [[project_broker_datastore]] / #30 v2). The broker captures it from the `/sign`
  response it already proxies (and the maker also keeps its own copy — component 1).
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
- **Finality, not just import (RBF/reorg).** Import can succeed against a mempool
  tx as `Tentative` (see Resolved), but tentative ≠ final — if the witness tx is
  **replaced (RBF) or dropped**, the allocation must be **reverted**. The queue
  keeps watching the witness txid after a tentative import and:
  - **Mined** → promote (`update_witnesses`), show as confirmed/spendable.
  - **Replaced/dropped** (esplora 404 / a conflicting tx confirms) → **revert the
    tentative allocation**, mark the item `reverted`, notify the user.
  - Treat received RGB as **pending (not spendable)** until the witness tx
    confirms — same posture as BTC.
- States per item: `pending → importing → tentative → confirmed | reverted | failed(reason)`.
  Surface an "incoming RGB (pending)" badge + a retry/dismiss affordance.
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
2. **Maker consignment persistence + `reconsign`** — store consignments the maker
   produces (maker.db); `reconsign` re-derives from stock as the fallback (and
   recovers the pre-persistence `81060df…` COLX as the first test).
3. **Dapp**: enqueue-on-settle (replace best-effort import) + persist consignment
   in history + re-import affordance.
4. **Broker stash service** — persistence + authed recovery endpoint (with the
   Postgres datastore); largest, do last.

## Safety: RBF / replacement / finality

- The **swap tx can't be unilaterally RBF'd**: all inputs are signed `SIGHASH_ALL`,
  so changing anything invalidates the counterparty's signature (can't forge it).
- A party **can** double-spend its **own** input via a conflicting/RBF tx, which
  **evicts** the swap tx. This is an **abort, not theft**: neither output confirms,
  the maker's RGB input is freed (maker keeps its RGB), the taker reclaims its BTC.
- Therefore the only real hazard is a recipient treating a **tentative** allocation
  as final. Mitigation lives in the import queue: **received RGB is pending until
  confirmed; revert on replacement** (above).
- Design knobs to consider: have the maker broadcast the swap tx with a
  **non-RBF sequence** (reduce the replacement surface) and/or **CPFP** to speed
  confirmation; require N confs before the dapp marks a swap "settled" vs
  "broadcast". (The maker likewise shouldn't treat its incoming BTC as final until
  confirmed — same rule, both directions.)

## Resolved

- **Does RGB accept against a mempool (unconfirmed) witness tx?** YES — verified
  2026-06-07 in `rgb-wasm`: `parse_ords` maps a height-less tx to
  `WitnessOrd::Tentative` (only `height > 0` → `Mined`), and `accept_consignment`
  validates + `accept_transfer`s with `Tentative` (`Validity::Valid`). So import
  works pre-confirmation (asset lands tentative; `update_witnesses` promotes it
  once mined). **Confirmation is NOT a gate** for the import *action* — but
  tentative ≠ final: the queue must still watch for confirmation (promote) and
  **replacement/drop (revert)** — see Safety: RBF. (Corollary: the earlier
  stranded COLX was just never imported — `acceptConsignment` wasn't called — not
  a confirmation problem.)

## Open questions

- Auth model for the stash recovery endpoint (ownership proof vs per-quote token).
- Stash retention policy + dedupe between maker-local and broker copies.
- Maker `reconsign` for blinded seals (can't re-derive) — is that ever needed for
  swaps, or are swaps always witness-vout?
