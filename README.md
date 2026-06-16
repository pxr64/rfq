# RGB RFQ

Rust workspace for an RGB↔BTC RFQ atomic-swap architecture. The RGB swap path is real — tapret close method via `rgb-api`/`bp-wallet`, driven by the `colorex` operator CLI (wallets, token issuance, maker daemon). Some pieces remain scaffolded/mocked (Lightning, a matching engine, maker pricing beyond a fixed markup).

## Crates

- `rfq-types`: shared dependency-light DTOs and domain types; depends only on `serde`.
- `rfq-core`: request validation, quote expiry, and quote sorting helpers.
- `rfq-router`: async maker connector trait and concurrent RFQ fanout.
- `rfq-rgb`: RGB backend — the real `LibRgbBackend` (rgb-api/bp-wallet): tapret swaps, wallet creation, NIA issuance + distribution — plus a mock adapter for tests.
- `rfq-btc`: Bitcoin client trait + an electrum-backed implementation (tx broadcast, UTXO + fee-rate queries).
- `rfq-store`: in-memory quote storage.
- `rfq-maker`: maker implementation — RFQ quoting (fixed 1% markup for now), inventory + reservation lifecycle, and real swap settlement.
- `rfq-api`: Axum HTTP API exposing RFQ and quote acceptance endpoints.
- `rfq-client`: thin public HTTP SDK over `rfq-api`.
- `rfq-wallet`: Rust-native wallet toolkit — name-keyed wallet resolution, config, and interactive setup over `rfq-rgb` (shared by the maker + taker CLIs).
- `maker-node`: the `colorex` operator binary — maker daemon + `wallet` + `issuer` tooling (see the colorex CLI section).
- `taker-cli`: the `colorex-taker` binary — drives buy/sell atomic swaps through the broker.

## Boundaries

- `rfq-types` stays dependency-free except `serde`.
- `rfq-core` does not depend on wallet, RGB, API, or node crates.
- `rfq-api` does not depend on `rfq-wallet`.

## Commands

```bash
cargo check --workspace
cargo test --workspace
cargo run -p rfq-api
cargo run -p maker-node --bin colorex -- maker health
```

## colorex operator CLI

`colorex` (the `maker-node` binary) is the Rust-native operator toolkit — no
`rgb-cmd`, no docker. It spans the whole asset lifecycle (create wallets, mint
tokens, run a maker, trade) and is network-agnostic: `regtest` / `signet` /
`testnet` / `mainnet`. Swaps use the **tapret** close method (taproot, keychain-10),
so on-chain a swap looks like an ordinary P2TR spend.

Run via `cargo run -p maker-node -- <args>` (or `cargo install --path
services/maker-node` then `colorex <args>`). The taker is the separate `colorex-taker`
binary (`tools/taker-cli`).

### Quickstart — run a maker from scratch (signet)

End to end, a new maker is seven steps. Each links to the deep guide; the worked
examples use `signet` but every command takes `--network`.

1. **Build the binary.** `cargo install --path services/maker-node` (then `colorex …`),
   or run inline with `cargo run -p maker-node -- …`.
2. **Stand up chain access** — a signet `bitcoind` + a romanz `electrs`:
   `docker compose -f infra/signet/maker-chain.docker-compose.yml up -d --build`
   (electrs on `127.0.0.1:60601`). See [running-a-maker.md §0](docs/running-a-maker.md).
3. **Bootstrap the maker** — `colorex maker init` writes `~/.config/colorex/maker.toml`
   + node key, **creates the RGB wallet + signing account**, and prints a keychain-10
   address to fund.
4. **Fund + sync** — send signet coins to that address, then
   `colorex wallet sync --name maker --network signet --electrum 127.0.0.1:60601`.
5. **Get inventory** — either **self-issue** (you are the issuer):
   `colorex issuer issue --name maker --network signet --ticker FOO --asset-name "Foo" --precision 2 --supply 1000000`
   then register it `colorex maker contract import rgb:<id>`; **or receive** from a
   separate issuer (`maker wallet invoice` → their `issuer transfer` →
   `colorex maker contract import rgb:<id> --consignment <file|base64>`). See
   [issuing-tokens.md](docs/issuing-tokens.md).
6. **Price it** — `colorex maker order create --side buy --price <sats-per-unit> --size <units>`
   (`--side` is the *taker's* side: `buy` = you sell RGB).
7. **Run it** — `colorex maker up` (dials the broker from the config and auto-registers).

The two guides below expand each step. The command tables that follow are the full reference.

### `colorex maker` — the maker daemon

| Command | Purpose |
|---|---|
| `maker init` | Interactive setup: writes the config + node key, **creates the RGB wallet + signing account**, and prints a keychain-10 address to fund. One-shot. |
| `maker up` | Start the daemon: HTTP quote server + cleanup / rebalance / chain-observer + strategy loops. Loads standing orders for pricing. |
| `maker health` | Probe the broker. |
| `maker inventory [--btc]` | Print the per-contract RGB inventory snapshot (`--btc` for the BTC-pool + drift diagnostic). |
| `maker order create --side --price --size [--asset] [--mirror --mirror-spread-bps]` | Create/replace the standing order (price the maker quotes) for an (asset, side). `--price` is sats per whole token (a quote of `amount` smallest units costs `price * amount / 10^precision`). |
| `maker order list` / `cancel <id>` / `clear` | List / cancel one / clear all standing orders (persisted to `maker.db`). |
| `maker contract import <id> [--consignment <file\|base64>]` / `list` / `remove <id>` | Manage the tradeable-asset **registry** (lives in `maker.db`; replaced the old `[rgb] contract_id`). |
| `maker wallet …` | Wallet + funding ops — see the next table. |
| `maker reconsign --outpoint <txid:vout>` / `maker consignment --quote-id <id>` | Recovery: re-derive / re-serve a consignment a recipient lost. |

### `colorex maker wallet` — the maker's wallet + funding ops

| Command | Purpose |
|---|---|
| `maker wallet addresses` | Print the BTC (keychain 0) + RGB-anchor (keychain 10) addresses. Offline. |
| `maker wallet balances [--electrum]` | Funded sats per keychain (syncs against electrum). |
| `maker wallet invoice --amount [--contract]` | Mint an RGB invoice to receive inventory from an issuer. |
| `maker wallet accept --consignment <file\|base64> [--contract]` | Accept an incoming consignment into the maker's stash. |
| `maker wallet transfer --invoice [--fee] [--out]` | **Send** RGB from the maker's inventory to a recipient invoice (build + sign + broadcast). Run with the daemon stopped. |
| `maker wallet rescan [--electrum]` | Full from-scratch wallet rescan (recovers stranded tapret outputs). Daemon stopped. |
| `maker wallet recover [--contract] [--dry-run] [--fee]` | Sweep stranded RGB allocations into a fresh anchor. Daemon stopped. |

### `colorex wallet` — taproot RGB wallets (any role)

| Command | Purpose |
|---|---|
| `wallet create --network --data-dir --name --account-file` | Create a fresh taproot (tapret) RGB wallet + empty stock. |
| `wallet address --network --data-dir --name [--btc]` | Print a receive address to fund manually (default keychain-10 RGB; `--btc` = keychain-0). |
| `wallet sync --network --data-dir --name --electrum` | Sync against electrum after funding confirms. |
| `wallet balance --network --data-dir --name --electrum` | Sync + print the per-keychain BTC balance. |
| `wallet invoice --contract --amount` | Mint a witness-vout RGB receive invoice for a contract. |

### `colorex issuer` — mint + distribute tokens

| Command | Purpose |
|---|---|
| `issuer issue --ticker --asset-name --precision --supply [--details] [--seal] [--issuer]` | Mint a Non-Inflatable Asset (fixed-supply fungible). Prints the contract id. Omit `--seal` to auto-pick a funded keychain-10 UTXO. |
| `issuer contracts --network --data-dir --name` | List issued contracts. |
| `issuer transfer --invoice --electrum --account-file [--fee]` | Distribute tokens to a recipient's RGB invoice (signs + broadcasts; prints a consignment for the recipient to accept). |

### Guides

- [docs/issuing-tokens.md](docs/issuing-tokens.md) — create a wallet, fund + sync,
  mint a token, and distribute it to a recipient (the wallet + issuance half of the quickstart).
- [docs/running-a-maker.md](docs/running-a-maker.md) — run a maker from `init`
  through inventory and standing orders to a live daemon (the maker half).

## Regtest RGB Infra

The first on-chain RGB20/NIA dev stack lives under `infra/regtest`.

```bash
make -C infra/regtest regtest-up
make -C infra/regtest regtest-mine BLOCKS=103
make -C infra/regtest rgb-tools-install
```

See `docs/regtest-rgb20-nia-dev-infra.md` for the manual issue and transfer checklist.
