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
crates/maker-node` then `colorex <args>`). The taker is the separate `colorex-taker`
binary (`crates/taker-cli`).

### `colorex maker` — the maker daemon

| Command | Purpose |
|---|---|
| `maker init` | Interactive setup: writes the config + node key, **creates the RGB wallet + signing account**, and prints a keychain-10 address to fund. One-shot. |
| `maker up` | Start the daemon: HTTP quote server + cleanup / rebalance / chain-observer loops. Loads standing orders for pricing. |
| `maker health` | Probe the broker. |
| `maker inventory` | Print the RGB inventory snapshot. |
| `maker invoice --amount` | Mint an RGB invoice to receive inventory from an issuer. |
| `maker order create --side --price --size [--asset]` | Create/replace the standing order (price the maker quotes) for an (asset, side). |
| `maker order list` / `maker order cancel <id>` | List / cancel standing orders (persisted to `orders.json` next to the config). |

### `colorex wallet` — taproot RGB wallets (any role)

| Command | Purpose |
|---|---|
| `wallet create --network --data-dir --name --account-file` | Create a fresh taproot (tapret) RGB wallet + empty stock. |
| `wallet address --network --data-dir --name [--btc]` | Print a receive address to fund manually (default keychain-10 RGB; `--btc` = keychain-0). |
| `wallet sync --network --data-dir --name --electrum` | Sync against electrum after funding confirms. |

### `colorex issuer` — mint + distribute tokens

| Command | Purpose |
|---|---|
| `issuer issue --ticker --asset-name --precision --supply [--details] [--seal] [--issuer]` | Mint a Non-Inflatable Asset (fixed-supply fungible). Prints the contract id. Omit `--seal` to auto-pick a funded keychain-10 UTXO. |
| `issuer contracts --network --data-dir --name` | List issued contracts. |
| `issuer transfer --invoice --electrum --account-file [--fee]` | Distribute tokens to a recipient's RGB invoice (signs + broadcasts; prints a consignment for the recipient to accept). |

### Guides

- [docs/issuing-tokens.md](docs/issuing-tokens.md) — create a wallet, fund + sync,
  mint a token, and distribute it to a recipient (any network).
- [docs/running-a-maker.md](docs/running-a-maker.md) — run a maker from `init`
  through inventory to standing orders.

## Regtest RGB Infra

The first on-chain RGB20/NIA dev stack lives under `infra/regtest`.

```bash
make -C infra/regtest regtest-up
make -C infra/regtest regtest-mine BLOCKS=103
make -C infra/regtest rgb-tools-install
```

See `docs/regtest-rgb20-nia-dev-infra.md` for the manual issue and transfer checklist.
