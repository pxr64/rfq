# Regtest RGB20/NIA Dev Infra

This document tracks the first reproducible infrastructure path for an on-chain RGB RFQ MVP. It uses Bitcoin regtest, an Electrum backend, and RGB command-line tooling before any real RGB code is wired into `rfq-rgb` or `maker-node`.

The product language can keep saying RGB20, but the current RGB tooling names the fungible non-inflatable asset schema `NonInflatableAsset` / NIA.

## Local Stack

The Docker stack lives in `infra/regtest` and starts:

- `bitcoind` in regtest mode with RPC, txindex, and ZMQ.
- `electrs` against the regtest node, exposing Electrum on `localhost:50001`.
- persistent Docker volumes for Bitcoin chain state and Electrum index state.

Run from the repo root:

```bash
make -C infra/regtest regtest-up
make -C infra/regtest regtest-mine BLOCKS=103
```

Stop or reset:

```bash
make -C infra/regtest regtest-down
make -C infra/regtest regtest-reset
```

## Tooling

Install pinned RGB sandbox-compatible tools:

```bash
make -C infra/regtest rgb-tools-install
```

This installs:

- `bp-wallet 0.11.1-alpha.2` under `infra/regtest/tools/bp-wallet`
- `rgb-cmd 0.11.1-rc.6` under `infra/regtest/tools/rgb-cmd`

These tools are intentionally kept outside the Rust workspace dependency graph. Real RGB dependencies still belong only behind `rfq-rgb` once we add an adapter.

Fetch the standard RGB schemata (needed for the NIA / non-inflatable-asset schema import):

```bash
make -C infra/regtest rgb-schemas-fetch
```

This clones `RGB-WG/rgb-schemata` into `infra/regtest/tools/rgb-schemas` and checks out the `v0.11.1` branch (which matches the pinned `rgb-cmd 0.11.1-rc.6`; the `master` branch is on the older `v0.11.0-beta.9` line and produces `entity not found` errors during import). The NIA schema file ends up at `infra/regtest/tools/rgb-schemas/schemata/NonInflatableAsset.rgb`. Override the upstream via `RGB_SCHEMAS_REPO=` and the ref via `RGB_SCHEMAS_REF=` to pin a different commit/tag/branch.

## Manual Happy Path Checklist

Set convenience variables (works from any cwd inside the repo; `REGTEST_DIR` anchors all paths to the regtest infra dir via the git root):

```bash
export REGTEST_DIR="$(git rev-parse --show-toplevel)/infra/regtest"
export ELECTRUM_URL="localhost:50001"
export WALLET_PATH="$REGTEST_DIR/wallets"
export ARTIFACTS="$REGTEST_DIR/artifacts"
export DATA="$REGTEST_DIR/data"
export TOOLS="$REGTEST_DIR/tools"
export SCHEMATA_DIR="$TOOLS/rgb-schemas/schemata"
export CONSIGNMENT="consignment.rgb"
export PSBT="tx.psbt"
export CLOSING_METHOD="opret1st"

alias bcli="docker compose -f $REGTEST_DIR/docker-compose.yml exec -T bitcoind bitcoin-cli -regtest -datadir=/home/bitcoin/.bitcoin"
alias bp="$TOOLS/bp-wallet/bin/bp"
alias bphot="$TOOLS/bp-wallet/bin/bp-hot"
alias rgb_issuer="$TOOLS/rgb-cmd/bin/rgb -n regtest --electrum=$ELECTRUM_URL -d $DATA/issuer -w issuer"
alias rgb_maker="$TOOLS/rgb-cmd/bin/rgb -n regtest --electrum=$ELECTRUM_URL -d $DATA/maker -w maker"
alias rgb_taker="$TOOLS/rgb-cmd/bin/rgb -n regtest --electrum=$ELECTRUM_URL -d $DATA/taker -w taker"
```

Sanity-check the paths resolve (run `make rgb-tools-install` and `make rgb-schemas-fetch` first if any of these are missing):

```bash
test -x "$TOOLS/rgb-cmd/bin/rgb"                  && echo "rgb-cmd ok"
test -x "$TOOLS/bp-wallet/bin/bp-hot"             && echo "bp-hot ok"
test -f "$SCHEMATA_DIR/NonInflatableAsset.rgb"    && echo "NIA schema ok"
```

Prepare Bitcoin funds:

```bash
bcli createwallet miner
bcli -rpcwallet=miner generatetoaddress 103 "$(bcli -rpcwallet=miner getnewaddress "" bech32)"
```

Create three independent wallet/stash roles:

- issuer: creates the RGB asset
- maker: receives RFQ inventory
- taker: creates the accept-side invoice and imports the final consignment

Generate fresh `bp-hot` seeds and bip84 P2WPKH account descriptors for all three roles. They are persisted under `$WALLET_PATH`:

```bash
make -C infra/regtest rgb-wallets-init
```

This writes `$WALLET_PATH/<role>.seed`, `<role>.account`, and `<role>.descriptor` for each of `issuer`, `maker`, `taker`. The script is idempotent — re-running it reuses existing seeds. `make regtest-reset` wipes `$WALLET_PATH`, after which the next invocation generates new seeds.

The current `bp-hot 0.11.1-alpha.2` only generates random seeds (no mnemonic-import flag), so descriptors change across `regtest-reset` cycles. The seed files use an empty password (regtest only — never reuse the layout for real funds). Save `$WALLET_PATH/<role>.seed` if you want continuity across resets.

Each `<role>.descriptor` file holds a full `wpkh` inner descriptor — `[fingerprint/derivation]xpub/<0;1;9>/*` — which is what `rgb create --wpkh` expects. The terminal `/<0;1;9>/*` selects the external chain (`0`), the change chain (`1`), and the RGB seal-anchor chain (`9` — needed for `rgb address -k 9`):

```bash
rgb_issuer create --wpkh "$(cat "$WALLET_PATH/issuer.descriptor")" issuer
rgb_maker  create --wpkh "$(cat "$WALLET_PATH/maker.descriptor")"  maker
rgb_taker  create --wpkh "$(cat "$WALLET_PATH/taker.descriptor")"  taker
```

Import the NIA schema into all RGB stashes:

```bash
rgb_issuer import "$SCHEMATA_DIR/NonInflatableAsset.rgb"
rgb_maker import "$SCHEMATA_DIR/NonInflatableAsset.rgb"
rgb_taker import "$SCHEMATA_DIR/NonInflatableAsset.rgb"

rgb_issuer schemata
schema_id="<schema-id-from-output>"
```

Fund Bitcoin UTXOs for issuer/maker/taker receive seals (keychain 9) and sync each wallet:

```bash
make -C infra/regtest rgb-fund-wallets
```

This derives one keychain-9 address per role, sends 1 BTC from the miner wallet to each, mines a confirmation block, and runs `utxos --sync` for all three. Override the per-role amount via `RGB_FUND_AMOUNT=` (e.g. `RGB_FUND_AMOUNT=2 make -C infra/regtest rgb-fund-wallets`).

From the `--- issuer ---` section of the printed output, pick the keychain-9 row and copy its outpoint:

```bash
outpoint_issue="<txid>:<vout>"   # from the issuer utxos table, keychain=9 row
```

Issue the NIA/RGB20-like asset:

```bash
# Render the contract YAML from the selected NIA schema id, issued supply, and issuer outpoint.
# The exact YAML shape follows the rgb-cmd/sandbox contract template in use.

rgb_issuer issue "ssi:issuer" "$ARTIFACTS/rfq-nia.yaml"
contract_id="<contract-id-from-output>"

rgb_issuer contracts
rgb_issuer state "$contract_id"
```

Transfer inventory from issuer to maker:

```bash
rgb_maker invoice --amount 1000 "$contract_id"
maker_invoice="<invoice-from-output>"

rgb_issuer transfer "$maker_invoice" "$DATA/issuer/maker.$CONSIGNMENT" "$DATA/issuer/maker.$PSBT"
cp "$DATA/issuer/maker.$CONSIGNMENT" "$DATA/maker/$CONSIGNMENT"

bcli -rpcwallet=miner generatetoaddress 1 "$(bcli -rpcwallet=miner getnewaddress "" bech32)"

rgb_issuer utxos --sync
rgb_maker utxos --sync
rgb_maker accept "$DATA/maker/$CONSIGNMENT"
rgb_maker state "$contract_id"
```

Transfer from maker to taker, matching the RFQ accept path:

```bash
rgb_taker invoice --amount 100 "$contract_id"
taker_invoice="<invoice-from-output>"

rgb_maker transfer "$taker_invoice" "$DATA/maker/taker.$CONSIGNMENT" "$DATA/maker/taker.$PSBT"
cp "$DATA/maker/taker.$CONSIGNMENT" "$DATA/taker/$CONSIGNMENT"

bcli -rpcwallet=miner generatetoaddress 1 "$(bcli -rpcwallet=miner getnewaddress "" bech32)"

rgb_maker utxos --sync
rgb_taker utxos --sync
rgb_taker accept "$DATA/taker/$CONSIGNMENT"
rgb_taker state "$contract_id"
rgb_maker state "$contract_id"
```

Success means:

- issuer has a known NIA contract id
- maker has a visible owned RGB allocation before the RFQ-like transfer
- taker imports and validates a consignment
- taker sees the received amount in `rgb_taker state "$contract_id"`
- maker sees the remaining/change amount in `rgb_maker state "$contract_id"`

## Known Constraints & Version Pins

Gotchas baked into the stack today. If you bump any pinned version, re-check this list before declaring the upgrade clean.

### Indexer

- **electrs must be a romanz build, not a blockstream-fork derivative.** `bp-wallet 0.11.1-alpha.2`'s electrum parser is brittle to `blockchain.transaction.get` response-shape differences: against `mempool/electrs` or `getumbrel/electrs` it trips an internal `expect("broken logic")` at `src/indexers/electrum.rs:220` during keychain sync. We work around this by building `romanz/electrs` v0.11.1 from source in [electrs.Dockerfile](../infra/regtest/electrs.Dockerfile).
- **`-v` flag is rejected by romanz/electrs v0.11.1** even though `--help` advertises it. Use `--log-filters=INFO` instead (already set in `docker-compose.yml`).

### bitcoind auth

- **bitcoind uses cookie auth, not `-rpcuser`/`-rpcpassword`.** romanz/electrs 0.11.1 only supports `--cookie-file=<path>` (no `--cookie=user:pass` flag). If both `-rpcuser` AND `--cookie-file` are set, bitcoind suppresses cookie generation and electrs fails to start.
- **`bitcoin-cli` running inside the container needs `-datadir=/home/bitcoin/.bitcoin`.** `docker compose exec` runs as root, but bitcoind runs as the `bitcoin` user (uid 101) and writes `.cookie` under `/home/bitcoin/.bitcoin/regtest/`. Without the `-datadir` flag, bitcoin-cli looks at `/root/.bitcoin/regtest/.cookie`, doesn't find it, and bails with "Could not locate RPC credentials." Already applied to the healthcheck and the `bcli` helpers.

### Wallet descriptor

- **Multipath terminal must declare keychain 9 explicitly.** rgb-cmd uses keychain 9 for RGB seal anchors (`rgb address -k 9`). BIP-389 multipath descriptors are strict — `/<0;1>/*` would reject derivation at index 9. The `rgb-wallets-init` script writes `/<0;1;9>/*` so receive (0), change (1), and seal-anchor (9) chains are all valid.
- **`--electrum=` baked empty into shell aliases stays empty.** Shell `alias` definitions evaluate `$VAR` at definition time. If you paste the alias block while `$ELECTRUM_URL` is unset, the alias captures `--electrum=` with no value; later setting `ELECTRUM_URL=...` doesn't fix it. Stash-only commands (`contracts`, `schemata`, `inspect`) still work; anything that needs the indexer (`address`, `utxos --sync`, `invoice`, `transfer`, `finalize -p`) fails with "invalid socket address." Recovery: `unalias rgb_issuer rgb_maker rgb_taker bcli bp bphot` then re-paste the export block with `ELECTRUM_URL` set first.

### Tooling versions

- **bp-hot 0.11.1-alpha.2 only generates random seeds** — no `--import-mnemonic` flag. `regtest-reset` regenerates fresh seeds, so issuer/maker/taker addresses (and outpoints) change after every reset. Back up `infra/regtest/wallets/*.seed` if you want continuity.
- **rgb-schemata must be checked out on the `v0.11.1` branch**, not `master`. `master` tracks `v0.11.0-beta.9` schemata, which fail rgb-cmd 0.11.1-rc.6 import with `Error: entity not found`. The `rgb-schemas-fetch` make target pins `v0.11.1`; override via `RGB_SCHEMAS_REF=<ref>` if you need a different version. Note that the v0.11.1 branch uses singular filenames (`NonInflatableAsset.rgb`); master uses plural (`NonInflatableAssets.rgb`).
- **Stable contract id requires a stable contract YAML AND a stable seal outpoint.** Even if [artifacts/rfq-nia.yaml](../infra/regtest/artifacts/rfq-nia.yaml) is byte-identical, re-issuing produces a different contract id because the genesis hash bakes in a creation timestamp (visible as the date column in `rgb_issuer contracts`). To reuse a contract id across resets you'd need to back up `data/issuer/regtest/*.dat` along with the wallet.

### Operational

- **`make regtest-reset` wipes `wallets/`, `data/`, `artifacts/`, and `contracts/generated/`.** It does not wipe `tools/` (so the cargo-installed binaries survive). Use `regtest-down` if you want to stop services without losing state.
- **`docker compose down -v` wipes ALL Docker volumes** (bitcoind chain state + electrs index). Prefer `docker compose down` (no `-v`) and selective `docker volume rm regtest_electrs-data` if you only want to reset the index.
- **`make regtest-up` now does `compose up -d --build`** so Dockerfile edits are picked up automatically. Cached builds skip in seconds; clean rebuilds take ~5-10 min (the cargo + dep compile inside the electrs builder stage).

## RFQ Integration Notes

The next implementation should add a CLI-backed adapter while preserving crate boundaries:

- `rfq-rgb`: invokes RGB tooling to list allocations, validate invoices, and create transfers.
- `rfq-wallet`: invokes wallet tooling for PSBT/key operations as needed.
- `maker-node`: maps real RGB inventory into `Allocation` records and calls the real adapter on quote accept.
- `rfq-api`, `rfq-router`, `rfq-core`, and `rfq-types`: remain free of concrete RGB dependencies.

Normal Rust tests must remain independent of Docker and RGB tools. Real regtest tests should be ignored/manual until the stack is stable.
