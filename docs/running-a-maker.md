# Running a maker with `colorex`

A maker quotes RFQs and settles atomic BTC↔RGB swaps. This walks the full
lifecycle: chain backend → bootstrap → fund → inventory → standing orders → live
daemon. The maker itself is Rust-native via `colorex` (no `rgb-cmd`); the only
Docker is an optional bitcoind + electrs chain backend. Network-agnostic
(`regtest` / `signet` / `testnet` / `mainnet`); the examples below are signet.

> **Status.** Commands + flags are verified against the current CLI, and the full
> signet round-trip (issue → fund → swap) has been exercised live.

## 0. Chain backend — bitcoind + electrs

A maker needs its own signet chain access: a `bitcoind` (signet) and a **romanz**
`electrs`. romanz specifically — the maker's bp-wallet + RGB resolver need the
verbose `blockchain.transaction.get` that the blockstream/mempool electrs forks
don't serve. The bundled compose runs just these two:

```bash
docker compose -f infra/signet/maker-chain.docker-compose.yml up -d --build
```

- `bitcoind` (signet) stays internal — electrs reaches it over the compose network.
- `electrs` is published on **`127.0.0.1:60601`** — the electrum endpoint you give
  `maker init` below.

⚠️ Signet **initial sync takes a while**; electrs only answers wallet queries once
bitcoind has caught up. Watch progress:

```bash
docker compose -f infra/signet/maker-chain.docker-compose.yml logs -f bitcoind electrs
```

You do **not** run the broker — you connect to one (e.g. Colorex signet at
`https://rfq-signet.colorex.io`). Run your own broker only for a fully local setup.

## 1. Bootstrap — `maker init`

```bash
colorex maker init
```

Interactive. It generates a node-identity keypair, prompts for network / broker
URL / listen addr / RGB params, **creates the RGB taproot wallet + signing
account** (one-shot — skipped if they already exist), and writes:

- `~/.config/colorex/maker.toml` — the daemon config (`[maker]` + `[rgb]`).
- `node.key` / `node.pub` — the node identity.

Answer the prompts with:

- **network** — `signet`
- **broker URL** — the network you're joining, e.g. `https://rfq-signet.colorex.io`
  (the maker derives `wss://…/maker-stream` from it). Use `http://127.0.0.1:3000`
  only if you're also running a local broker.
- **electrum URL** — `tcp://127.0.0.1:60601` (the electrs from step 0). Your local
  romanz electrs is **plaintext TCP** — no TLS on loopback. The URL takes a
  `tcp://`/`ssl://` scheme; no prefix also means tcp, so bare `127.0.0.1:60601`
  works too. `ssl://` is only for remote TLS servers (e.g. the signet default
  `ssl://mempool.space:60602`, which is fine for BTC but breaks the RGB resolver
  — so a maker uses its own romanz electrs over tcp).

It prints a **keychain-10** address to fund. There is no contract-id prompt — the
assets a maker trades live in a registry (`maker.db`), populated in step 3 with
`colorex maker contract import`.

## 2. Fund + sync

Send coins to the printed keychain-10 address, wait for a confirmation, then:

```bash
colorex wallet sync \
  --network <net> --data-dir <rgb-data-dir> --name <wallet-name> \
  --electrum <host:port>
```

(`<rgb-data-dir>` / `<wallet-name>` are the values you gave `init`.)

Two helpers:

```bash
colorex maker wallet addresses   # the BTC (keychain 0) + RGB-anchor (keychain 10) addresses — offline, no chain
colorex maker wallet balances    # the same, with funded sats per keychain (syncs against electrum)
```

`addresses` never touches the chain, so it's instant even before electrs has
synced — use it to grab the address to fund; use `balances` to confirm the coins
landed.

## 3. Acquire inventory

A maker needs RGB tokens to sell. Two paths:

### A. Self-issue (maker is also the issuer) — simplest

Mint straight into the maker's own wallet (no invoice/transfer round-trip):

```bash
colorex issuer issue \
  --network <net> --data-dir <rgb-data-dir> --name <wallet-name> \
  --ticker FOO --asset-name "Foo Token" --precision 2 --supply 1000000
```

Register the printed `rgb:...` contract id so the maker trades it (the registry
lives in `maker.db`, not the TOML):

```bash
colorex maker contract import rgb:...
```

See [issuing-tokens.md](issuing-tokens.md) for detail.

### B. Receive from a separate issuer

Mint a receive-invoice (`--contract` defaults to the sole registered contract;
pass it explicitly if you trade several):

```bash
colorex maker wallet invoice --amount 1000000 --contract rgb:...
```

Hand that invoice to the issuer; they run `colorex issuer transfer` against it and
return a base64 **consignment**. Import it — in one step, this accepts the
consignment into the stash *and* registers the contract:

```bash
colorex maker contract import rgb:... --consignment consignment.b64
```

(Or, if the contract is already registered: `colorex maker wallet accept --path consignment.b64`.)
Once its anchoring tx confirms, `maker up` counts it as inventory (see
[issuing-tokens.md](issuing-tokens.md)).

### The contract registry

The assets a maker trades live in a registry in `maker.db` — there is no
`contract_id` in the TOML. Manage it with:

```bash
colorex maker contract list              # registered assets (ticker · precision · id)
colorex maker contract import rgb:...    # register one (must be in the stock; --consignment to accept first)
colorex maker contract remove rgb:...    # stop trading it (stock/inventory untouched)
```

The daemon seeds and quotes **every** registered contract; registry changes take
effect on the next `colorex maker up`.

## 4. Set standing orders (pricing)

Orders are the maker's liquidity terms — the price it quotes per (asset, side),
where **`side` is the taker's side** (the order answers "when a taker requests
side X, quote at price Y"). **Without an order for an (asset, side) the maker
declines** — it quotes only what you've explicitly priced (no flat fallback), so
set an order for every side you want to trade.

```bash
# Back taker BUYS of FOO (you sell RGB) at 250 sats/unit, up to 1,000 units/quote:
colorex maker order create --side buy --price 250 --size 1000 --asset rgb:FOO

# Back taker SELLS of FOO (you buy RGB, paying BTC) at 240 sats/unit, up to 2,000:
colorex maker order create --side sell --price 240 --size 2000 --asset rgb:FOO

colorex maker order list
colorex maker order cancel <id>
```

- `--side`: `buy` = the taker buys RGB (maker sells); `sell` = the taker sells RGB
  (maker buys, paying BTC from its BTC pool).
- `--price`: sats per **smallest RGB unit**.
- `--size`: the largest single quote (smallest RGB units) the order backs — a
  request above it is **declined**.
- `--asset`: optional; defaults to the sole registered contract (pass it
  explicitly if the maker trades more than one).
- `--mirror` / `--mirror-spread-bps`: **auto-mirror**. On each fill of this order
  the strategy loop places the *opposite-side* order at the fill price ∓ the spread,
  so inventory ping-pongs back toward neutral and the spread is your margin:

  ```bash
  # Sell FOO and auto-rebuy on each fill at 5% (500 bps) under the fill price:
  colorex maker order create --side buy --price 250 --size 1000 --mirror --mirror-spread-bps 500
  ```

  A `buy` fill (you sold) arms a cheaper `sell`; a `sell` fill (you bought) arms a
  dearer `buy`. Set a non-zero spread (the default `0` mirrors at the same price, no
  margin), and keep the daemon running — mirroring is done by the strategy loop in
  `maker up`.

Orders persist to `maker.db` (the maker's SQLite store, next to the config), and
`maker up` loads them. Creating a second order for the same (asset, side)
**replaces** the first.

> **Current limits (v1):** one standing order per (asset, side); `--size` is a
> per-quote cap, not a running fill total; actual fills are still bounded by
> on-hand inventory. Multi-order books + fill accounting are a follow-up.

## 5. Run the daemon

Just start the maker — it dials the broker from `maker.toml` and auto-registers:

```bash
colorex maker up
```

(Only run a broker yourself for a fully local setup: `cargo run -p rfq-api` in a
separate process, with `broker_url = http://127.0.0.1:3000`. Joining Colorex
signet, you skip this.)

The startup banner echoes `standing_orders=<n>`. The daemon serves quotes (priced
from your orders), and runs the cleanup / rebalance / chain-observer loops.

```bash
colorex maker health        # probe the broker
colorex maker inventory     # RGB inventory snapshot
```

A taker can now trade against it via `colorex-taker` (see `crates/taker-cli`).

## 6. Maintenance & recovery

These operate on the maker's wallet directly — **stop the daemon first** so they
don't contend over the wallet; the chain observer reconciles `maker.db` on the next
`maker up`.

```bash
# Send RGB from the maker's own inventory to a recipient's invoice (build+sign+broadcast):
colorex maker wallet transfer --invoice <recipient-invoice> --fee 1000 --out consignment.b64

# Sweep RGB stranded on tapret outputs bp-wallet's incremental scan missed:
colorex maker wallet recover --dry-run        # preview what would be swept
colorex maker wallet recover                  # actually sweep into a fresh anchor

# Full from-scratch wallet rescan (heavier than the daemon's incremental sync):
colorex maker wallet rescan
```

- **transfer** is the maker analogue of `issuer transfer` — the contract + amount
  come from the invoice; recipient accepts the printed consignment after the tx confirms.
- **recover** / **rescan** are for the tapret-output stranding case: if `maker inventory`
  shows less than you expect, `inventory --btc` diagnoses it (sellable vs stranded vs
  spent), then `recover` sweeps the stranded allocations back into spendable inventory.

## How pricing resolves

For each quote the maker consults its orders for (asset, side):

| Situation | Result |
|---|---|
| Matching order, amount ≤ size | quote at the order's price |
| Matching order, amount > size | **decline** the quote |
| No order for this (asset, side) | **decline** (no quote) |
