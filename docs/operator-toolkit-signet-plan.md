# Operator Toolkit & Signet Deployment Plan

**Status:** planning (agreed 2026-06-04). **Prerequisite DONE:** the opret→tapret
swap migration (regtest e2e 11/11; commits `5bc00bf`…`88f31f5`).

Companion to [broker-maker-node-roadmap.md](broker-maker-node-roadmap.md); this
doc covers the next phase: turning the working swap into something an **operator**
can run live on **signet** (then mainnet).

## Goal

A network-agnostic **`colorex` operator toolkit** spanning the full RGB asset
lifecycle — **issue → distribute → make markets → trade** — so a maker and a taker
can run live on signet with **no docker/regtest dependency**, implemented
**Rust-native** (via the `rgb-api` / `rgb-std` / `bp-wallet` crates we already
depend on), not `rgb-cmd` shell-outs.

## Why now

The tapret swap works end-to-end on regtest. The blockers to "run it on signet as
a real operator" are **tooling, not protocol**:

- Wallet creation + RGB issuance live only in docker/regtest shell scripts
  (`infra/regtest/scripts/*`, which shell out to `rgb-cmd` and require a local
  bitcoind+electrs). They can't run on a public network.
- The maker quotes at a **hardcoded 1% markup** (`amount × 101`,
  `rfq-maker/src/lib.rs:969`/`:466`) — there is **no liquidity/order management**.
- Three sites hardcode `BitcoinNetwork::Regtest` in the asset-id
  (`maker-node/src/lib.rs:286`, `taker-cli/src/main.rs:249` & `:304`).

What's already fine: the network is otherwise parameterized (`parse_network`,
`chain_net_for`, the electrum resolver, `colorex maker init`), and the swap TTLs
are **signet-safe** (2h broadcast→confirm window, `rfq-maker/src/lib.rs:89`), so
reservations won't expire across ~10-min blocks.

## Target surface (`colorex` operator toolkit)

| Role | Commands | Today |
|---|---|---|
| **wallet** (all roles) | `create` · `address` · `sync` | gap — docker/`rgb-cmd` script only |
| **issuer** | `issue` (create token + initial supply) · `transfer` (distribute) · *[mint — future]* | gap — docker script only |
| **maker** | `order create/list/cancel` · `up` · `inventory` | `up`/`inventory` exist; **orders = gap** |
| **taker** | `buy` · `sell` | exists (`taker-cli`) |

Open: whether `colorex` (maker-node) and `colorex-taker` (taker-cli) unify into one
binary with role subcommands, or share a common `wallet`/`issuer` command set. Lean
toward a shared command set.

## RGB issuance model (scope note)

- **NIA** (Non-Inflatable Asset — what we use): the **full supply is allocated at
  genesis** to the issuer's seal. So **"create a token" and "mint the initial
  supply" are one step**, and supply is then fixed.
- **Minting *more* later** needs an **inflatable** schema (RGB20-with-inflation /
  RGB25) where the issuer holds an inflation right — a separate, larger feature
  (different schema + inflation-right state + a mint operation). **Deferred.**

## Implementation direction: Rust-native

- No `rgb-cmd` shell-outs (aligns with issue #24). Use `rgb-api`/`rgb-std` for the
  Stock + contract issuance, and `bp-wallet` for seed/derive/descriptor/wallet
  creation — all already dependencies.
- The e2e harness already derives seeds/accounts in-Rust
  (`crates/rfq-rgb/src/test_helpers.rs`); extend that to wallet **creation** +
  **issuance** so colorex owns the whole flow. `LibRgbBackend` already has the
  transfer primitives.
- The docker `infra/regtest` bootstrap scripts stay for regtest convenience but are
  no longer the path for real networks.

## Build order

### Phase 1 — Network fixes (small)
Make the `AssetId` network come from config (not hardcoded `Regtest`):
`maker-node/src/lib.rs:286`, `taker-cli/src/main.rs:249` & `:304`; add a `network`
field to `taker.toml`. Without this, taker↔maker asset IDs mismatch on signet.

### Phase 2 — Wallet tooling (Rust-native)
- `colorex wallet create` — per role; taproot (bip86, keychain-10 / tapret), against
  the configured network + electrum; writes the rgb data dir + descriptor +
  encrypted account. No `rgb-cmd`/docker.
- `colorex wallet address` — print the keychain-10 receive address (to fund
  manually from a faucet).
- `colorex wallet sync` — refresh against electrum after funding confirms.

### Phase 3 — Issuer tooling (Rust-native)
- `colorex issuer issue --ticker --name --precision --supply` — issue an NIA
  contract bound to the issuer's funded signet UTXO → `contract-id`.
- `colorex issuer transfer --to <invoice> --amount` — distribute (seed the maker
  with RGB to sell).

### Phase 4 — Validate on signet
Fund issuer/maker/taker keychain-10 addresses from a **signet faucet** (manual;
~10-min confirmations). Issue the contract, transfer to the maker, run broker +
`colorex maker up` + taker buy/sell. Confirm a live tapret swap on signet.

### Phase 5 — Order / liquidity tooling (the real maker layer)
- `colorex maker order create --side {buy|sell} --asset <id> --price <sat/unit> --size <units>`
  — a **standing quote-offer**.
- `order list` / `order cancel <id>`.
- The RFQ handler matches an incoming request against standing orders
  (asset/side/`size ≤ remaining`) → quotes at the order's price; **declines** if
  none. Replaces the hardcoded `×101`.
- Orders are **backed by inventory** (sell ≤ available RGB; buy ≤ available BTC),
  **decremented on fill**, and **persisted** like the inventory store.
- Pricing per order starts as a **flat sat/unit**; a mid-from-feed + spread model
  can come later.

### Phase 6 — (future) inflatable assets / re-mint
Inflatable schema support + a `mint` op for secondary issuance.

## Signet operational notes

- **Public signet first** — public faucet + public electrum
  (`electrum.blockstream.info:60002`). A custom/controlled signet only if needed.
- **Manual funding** — colorex prints the address; the operator funds from a faucet.
  No automated faucet calls.
- **New `contract-id` per network** — genesis is UTXO-bound, so re-issue on signet.
- Confirmations are real (~10 min); the swap TTLs already accommodate this.
- **3-process run:** broker (`rfq-api`) + maker (`colorex maker up`) + taker
  (`taker-cli`), each configured with `network = "signet"`, the signet electrum,
  and the issued `contract-id`.

## Decisions captured

- Bootstrap approach: **enhance colorex** (Rust-native wallet + issuance), manual
  faucet funding — not the docker scripts. (User, 2026-06-04.)
- Network: **signet** (public signet first). (User, 2026-05-31.)
- Implementation: **Rust-native**, drop `rgb-cmd` shell-outs. (User, 2026-06-04.)
- Maker pricing: standing **orders** (`create_order`-style), not a fixed spread —
  it's how a maker manages liquidity. (User, 2026-06-04.)
