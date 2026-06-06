# Running a maker with `colorex`

A maker quotes RFQs and settles atomic BTC↔RGB swaps. This walks the full
lifecycle: bootstrap → fund → inventory → standing orders → live daemon. All
Rust-native via `colorex` (no `rgb-cmd`, no docker), network-agnostic
(`regtest` / `signet` / `testnet` / `mainnet`).

> **Status.** Commands + flags are verified against the current CLI. The full
> public-network round-trip is **not yet live-validated** (Phase 4). Treat the
> public-network steps as "should work", not a verified runbook.

## 1. Bootstrap — `maker init`

```bash
colorex maker init
```

Interactive. It generates a node-identity keypair, prompts for network / broker
URL / listen addr / RGB params, **creates the RGB taproot wallet + signing
account** (one-shot — skipped if they already exist), and writes:

- `~/.config/colorex/maker.toml` — the daemon config (`[maker]` + `[rgb]`).
- `node.key` / `node.pub` — the node identity.

It prints a **keychain-10** address to fund. Leave `RGB contract id` empty for
now if you haven't issued the asset yet — set it after step 3.

> The electrum URL here is a **bare** `host:port` (the daemon resolves via
> bp-electrum). There is no public Blockstream signet server — point signet at
> your own `electrs`.

## 2. Fund + sync

Send coins to the printed keychain-10 address, wait for a confirmation, then:

```bash
colorex wallet sync \
  --network <net> --data-dir <rgb-data-dir> --name <wallet-name> \
  --electrum <host:port>
```

(`<rgb-data-dir>` / `<wallet-name>` are the values you gave `init`.)

## 3. Acquire inventory

A maker needs RGB tokens to sell. Two paths:

### A. Self-issue (maker is also the issuer) — simplest

Mint straight into the maker's own wallet (no invoice/transfer round-trip):

```bash
colorex issuer issue \
  --network <net> --data-dir <rgb-data-dir> --name <wallet-name> \
  --ticker FOO --asset-name "Foo Token" --precision 2 --supply 1000000
```

Put the printed `rgb:...` contract id in `maker.toml` `[rgb] contract_id`. See
[issuing-tokens.md](issuing-tokens.md) for detail.

### B. Receive from a separate issuer

Set `contract_id` in `maker.toml` first, then mint a receive-invoice:

```bash
colorex maker invoice --amount 1000000
```

Hand that invoice to the issuer; they run `colorex issuer transfer` against it and
give you back a consignment to accept (see [issuing-tokens.md](issuing-tokens.md)).

## 4. Set standing orders (pricing)

Orders are the maker's liquidity terms — the price it quotes per (asset, side),
where **`side` is the taker's side** (the order answers "when a taker requests
side X, quote at price Y"). Without any order, the maker falls back to a flat
default (`DEFAULT_UNIT_PRICE_SATS`, ~1% over a 100-sat par).

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
  request above it is **declined**, not quoted at the fallback.
- `--asset`: optional; defaults to the config's `contract_id`.

Orders persist to `orders.json` next to the config, and `maker up` loads them.
Creating a second order for the same (asset, side) **replaces** the first.

> **Current limits (v1):** one standing order per (asset, side); `--size` is a
> per-quote cap, not a running fill total; actual fills are still bounded by
> on-hand inventory. Multi-order books + fill accounting are a follow-up.

## 5. Run the daemon

Start the broker, then the maker:

```bash
cargo run -p rfq-api        # the broker (separate process)
colorex maker up
```

The startup banner echoes `standing_orders=<n>`. The daemon serves quotes (priced
from your orders), and runs the cleanup / rebalance / chain-observer loops.

```bash
colorex maker health        # probe the broker
colorex maker inventory     # RGB inventory snapshot
```

A taker can now trade against it via `colorex-taker` (see `crates/taker-cli`).

## How pricing resolves

For each quote the maker consults its orders for (asset, side):

| Situation | Result |
|---|---|
| Matching order, amount ≤ size | quote at the order's price |
| Matching order, amount > size | **decline** the quote |
| No order for this (asset, side) | flat `DEFAULT_UNIT_PRICE_SATS` fallback |
