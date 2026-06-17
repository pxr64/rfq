# Consignment validation & SPV verification — design (for auditors)

Status: **implemented** (signet-deployed; mainnet-grade verifier, signet-gated PoW).
Audience: security reviewers / auditors of the Colorex RFQ swap stack.

This document describes how Colorex confirms that the RGB consignment backing a swap is
**real and settled on Bitcoin** before any BTC moves, and how it does so **without trusting
the broker, the indexer, or the consignment's own claims**. It is the design behind the
`rfq-consignment` crate, the maker/taker gates in `rfq-rgb`, the `spv-prover` service, and the
vendored verifier in the colorex-wallet (public) repo.

> Companion: the public client trust model is documented in
> `colorex-wallet/docs/spv-consignment-verification.md`. Internal rationale/history lives in
> `docs/consignment-validation-hardening-plan.md` and `docs/rfqip-1-spv-consignment-anchoring.md`.

## 1. What is being protected

RGB is a client-side-validation protocol: a token's ownership is proven by a **consignment** —
the history of state transitions, each anchored into a Bitcoin **witness transaction**. A swap
trades BTC for RGB atomically; the party receiving RGB commits real BTC against the consignment.

**The vulnerability (verified, then fixed):** RGB consensus validity is only `{Valid, Warnings}`
— it has **no "is it mined" gate**. A witness that is unmined, or *never broadcast*, still
validates as `Valid`. Worse, the standard accept path (`AnyResolver::add_consignment_txes`,
which the reference `rgb-cmd` calls unconditionally) serves the consignment's own witness txs
back from the blob hardcoded to `WitnessOrd::Tentative`, **never querying the chain**. So a
self-consistent but **fabricated** history validates, and a naive maker would pay BTC for RGB
that never existed. Closing this is the entire point of the machinery below.

## 2. Threat model

**Not trusted:**
- The **broker** — it relays quotes/consignments; its verdict is never authoritative.
- The **indexer** (electrs / esplora) — it can be wrong, compromised, or MITM'd.
- The **consignment's own claims** — the witness tx bytes it carries prove commitment, not
  inclusion.
- The **SPV prover** — a separate, replaceable service; its output is self-verifying.

**Trusted (the only roots):**
- **Bitcoin proof-of-work** + a small set of **checkpoints** baked into the verifier binary
  (auditable constants), for thin clients.
- A party's **own Bitcoin node** (electrs), for the maker/broker.
- **ICP subnet consensus** (native Bitcoin headers), for the canister.
- The **verifier binary itself** (you run it; it's open and reviewable).

**Principle:** every party that commits value validates *on its own node / with its own trust
root* before committing. The broker pre-check is defense-in-depth, never a substitute —
trusting it would merely relocate the vulnerability.

## 3. Two layers, one verdict

Confirming "every witness is mined" needs a Bitcoin chain source. There are two, split by who
runs a node:

| Consumer | Chain source | Module |
| --- | --- | --- |
| Maker, taker-cli, broker pre-check | live electrs | `rfq-consignment::mined` (`MinedChecker`) |
| Wallet (browser), ICP canister | self-fetched headers + merkle proofs (SPV proof-pack) | `rfq-consignment::{verify, headers, difficulty}` |

The trust-critical verifier core (`merkle`, `proofpack`, `verify`, `headers`, `difficulty`) is
**pure** — `sha2` + `serde` only, no electrum — so it compiles to native, wasm, and the ICP
canister, and is vendored into the (public) wallet.

## 4. The maker/taker gate (node-backed path)

In `rfq-rgb` (`lib_backend.rs`), both money gates — `validate_incoming_consignment` (sell) and
`validate_buy_consignment` (buy) — run **two passes**:

1. **Graph pass (seeded).** `consignment.validate(resolver, config)` with a resolver *seeded*
   via `add_consignment_txes`, asserting `Validity::Valid`. This proves the cryptography
   (transition graph, mpc/dbc commitments, seal closing, schema, AluVM) but says nothing about
   chain depth — every seeded witness reports `Tentative`. Seeding is *required*: on the buy side
   the terminal witness is the not-yet-broadcast swap tx, which an un-seeded `validate()` would
   fail to resolve (`ResolverError`).
2. **Mined-ancestry pass (chain-only).** Every witness in the validated `tx_ord_map` is
   re-resolved against electrs via `MinedChecker` (a *fresh, un-seeded* resolver) and required to
   be `Mined` ≥ K confirmations. Reject **before** `accept_transfer`, so a bad consignment never
   mutates the stash.

**Buy vs sell asymmetry:** the sell gate exempts nothing (the taker's whole provenance ancestry
must be mined); the buy gate exempts exactly one txid — the not-yet-broadcast swap tx
(`expected_witness_txid`).

**Hardening on this path:**
- **K is network-aware** (`Network::recommended_confs`: mainnet 6 / testnet 3 / signet+regtest 1).
- **Size cap** (`DEFAULT_MAX_WITNESSES` = 10,000): refuse an oversized ancestry before doing any
  per-witness work (DoS guard).
- **Stash bookmark**: witnesses confirmed ≥ `BURY_DEPTH` (100) deep are recorded
  (`<stock>/mined_bookmark`) and skipped on later gates — settled ancestry isn't re-walked.

## 5. SPV proof-packs (thin-client path)

A wallet/canister can't run a node. It verifies a **self-certifying** sidecar bundle:

```
SpvProofPack { version, network,
  anchors: { <txid> -> { block_hash, block_height, tx_index, merkle_proof[] } },
  headers: { <block_hash> -> <80-byte header hex> }   // optional; ignored by header-ful verifiers
}
```

- **Producer** (`spv-prover`, or the wallet self-fetching from esplora) is **untrusted**: a bad
  pack can only cause a verification *failure*, never a false accept.
- **Verifier** (`verify::verify_pack`) folds each witness's `merkle_proof` and checks the
  recomputed merkle root against a block header it gets from its **own** `HeaderSource`.

### Per-witness verification (the five checks)

For each witness txid (except the buy-side exempt swap tx):

1. **Anchor present** — a `WitnessInclusion` exists in the pack (else `MissingAnchor`).
2. **Header vouched** — the `HeaderSource` returns the block's real merkle root + depth for the
   claimed `(block_hash, height)` (else `UnknownHeader`).
3. **Inclusion** — fold txid up `merkle_proof` (display→internal byte order, `tx_index` directs
   left/right, double-SHA256); recomputed root must equal the header's (else `BadMerkle`).
4. **Depth** — ≥ K confirmations (else `Unmined`).
5. **Size cap** — the whole set is bounded by `DEFAULT_MAX_WITNESSES` first.

## 6. Header trust ladder (`HeaderSource`)

The verifier trusts **only** its header source. Three rungs (RFQIP-1 §3):

1. **Own validated chain** — ICP-native headers (subnet consensus already validated PoW). No
   checkpoint or difficulty logic needed; `header_at` just reads `bitcoin_get_block_headers`.
   *Most trustless, least verifier work.*
2. **Checkpoint-anchored self-validated** — `CheckpointHeaderSource`: the wallet fetches headers
   from esplora and **validates** them (§7, §8) against checkpoints baked into the binary.
3. ~~Trust a server's headers blindly~~ — **disallowed.**

`ElectrsHeaderSource` (node-backed) trusts the operator's own electrs and is used by the
prover/broker path.

## 7. `CheckpointHeaderSource` — what it validates

Given a contiguous run of 80-byte headers anchored at a baked `Checkpoint`:

- **Checkpoint anchor** — `headers[0]` must hash to the baked `(height, block_hash)`.
- **Linkage** — each header's `prev_block` = hash of the previous header.
- **Proof-of-work** — `dsha256(header) ≤ target(bits)` (network-gated).
- **Difficulty correctness** — §8.
- Then vouches for any block in the run with its merkle root + confirmation depth.

It rejects a pack that lies about a block's height, and (PoW networks) requires the checkpoint to
sit on a retarget boundary so difficulty validation has a complete epoch to anchor on.

## 8. Difficulty-retarget validation (`difficulty.rs`) — the critical piece

PoW-meets-stated-bits is **forgeable for free**: the attacker writes the `bits` field, so they
can claim minimum difficulty and mine a fake chain on a laptop. The fix is to **recompute** the
required difficulty and reject any header whose `bits` disagree:

- **Mid-epoch** (height not a 2016-multiple): `bits` must equal the previous block's.
- **At a retarget boundary** (every 2016 blocks): `bits` must equal
  `expected_retarget_bits(prev_bits, prev_time, first_of_epoch_time)` — Bitcoin's
  `CalculateNextWorkRequired` (clamp `actual/expected` to `[¼, 4×]`, scale the target, cap at the
  pow limit).

Because difficulty is a deterministic function of public prior-block timestamps, the verifier
re-derives it instead of trusting it. Forging now requires real Bitcoin-scale work.

**Validation of this module (consensus-exact, so triple-checked):**
- **Differential vs rust-bitcoin** — `compact_from_target`/`target_from_compact` match
  `bitcoin::Target` byte-for-byte across the valid range + real historical `bits`. `bitcoin` is a
  **dev-dependency only** — never shipped in the verifier (keeps wasm/canister lean).
- **Real mainnet vector** — `expected_retarget_bits` reproduces the exact `bits` Bitcoin set at
  block 840,672.
- **Invariants** — faster→harder, slower→easier, the 4× clamps, pow-limit cap.

**Network gating:** difficulty/PoW validation is currently **mainnet-only** (`Network::checks_pow`).
Signet blocks are secured by a signer signature (not header-only PoW) and regtest has none, so
those rely on checkpoint + linkage. *On signet the SPV path is therefore not fully trustless —
this is acceptable for a test network and is the reason the live deployment is signet.* (testnet's
20-minute rule and signet-signature validation are noted follow-ups.)

## 9. Dense multi-checkpoint + bounded runs (scaling)

A single checkpoint forces a bad tradeoff (genesis anchor → huge runs; recent anchor → can't
verify old ancestry). Instead, **one checkpoint per difficulty epoch** (every 2016 blocks; ~440
hashes ≈ 14 KB cover all of Bitcoin history) is baked in. To verify a witness at height `W`:

- `nearest_checkpoint(checkpoints, W)` selects the epoch-boundary anchor at/below `W`.
- Fetch + validate **only** the short run from that anchor to `W` — **≤ 2016 headers**, regardless
  of chain height or how long the client was offline.
- `from_segments(network, segments, tip_height)` validates each per-witness run independently and
  merges them into one `HeaderSource` (the real chain tip is passed separately for confirmation
  depth, since bounded runs don't reach it).

This decouples verification cost from chain height; per-witness work is bounded by one epoch.
*(The dense checkpoint table itself is data the wallet bakes in and refreshes ~every 2 weeks; the
mechanism is implemented + tested, the full table is a data-generation follow-up.)*

## 10. Code map & tests

| Concern | Location |
| --- | --- |
| Verifier core (pure) | `crates/rfq-consignment/src/{merkle,proofpack,verify,headers,difficulty}.rs` |
| Live mined check | `crates/rfq-consignment/src/mined.rs` (`MinedChecker`) |
| SPV prover (untrusted producer) | `crates/rfq-consignment/src/prove.rs`, `services/spv-prover` |
| Maker/taker gates | `crates/rfq-rgb/src/lib_backend.rs` (`validate_*_consignment`) |
| Broker pre-check + distribution | `services/broker/src/lib.rs` |
| Wallet wasm binding (vendored verifier) | `colorex-wallet/rgb-wasm/src/spv/`, `verify_consignment_spv` |

Test coverage lives next to each module: merkle byte-order vs the genesis vector; verifier accept
+ each `RejectReason`; header linkage/PoW/checkpoint/difficulty; the rust-bitcoin differential +
real mainnet retarget vector; a live regtest prover→verifier round-trip with tamper cases.

## 11. Residual trust & known limitations

- **Standard SPV assumptions** — the verifier trusts that its header source represents the
  most-work chain and that no reorg deeper than K occurs.
- **Checkpoint freshness** — the baked checkpoint table must be refreshed as new epochs occur; a
  stale table simply can't vouch for blocks past its last checkpoint (fails closed).
- **Signet is not fully trustless** (§8) — PoW gating is mainnet-only today.
- **RGB is pre-release** — the underlying `rgb-api` / `bp-*` are rc/alpha; that protocol risk is
  outside this verifier and is the primary reason real-value mainnet deployment awaits review.
- **Vendoring drift** — the wallet copy of the verifier must be kept in sync with this crate
  (enforced by review; see `colorex-wallet/rgb-wasm/src/spv/mod.rs`).
