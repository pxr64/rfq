# Issuing & distributing RGB tokens with `colorex`

End-to-end operator walkthrough: create a wallet, mint a Non-Inflatable Asset
(NIA), and distribute it to a recipient — all Rust-native via the `colorex` CLI
(no `rgb-cmd`, no docker).

`colorex` is **network-agnostic**: every command takes `--network`, valid values
`regtest` / `signet` / `testnet` / `mainnet`. The examples below use `signet` as a
concrete public-network case; swap it for any other network (and the matching
electrum endpoint) without changing the command shapes.

> **Status.** Every command + flag below is verified against the current CLI
> (`services/maker-node/src/main.rs`), and the issue → fund → distribute round-trip
> has been exercised live on signet.

## Prerequisites

- A built `colorex` binary: `cargo install --path services/maker-node` (or run via
  `cargo run -p maker-node -- <args>`).
- An **electrum** endpoint for your network. Mind the two URL forms (see below).
- A way to fund addresses manually — a faucet on a test network, or real coins on
  mainnet. We never call a faucet automatically. Confirmations on public networks
  take ~10 min.

### Electrum URL forms (important)

`colorex` talks to electrum through **two different clients**, which currently
disagree on URL format:

| Path | Client | URL form |
|---|---|---|
| `wallet sync`, maker daemon | bp-electrum resolver | **bare** `host:port` (e.g. `localhost:60001`) |
| `issuer transfer` broadcast | electrum-client | **scheme** `tcp://host:port` or `ssl://host:port` |

So pass `--electrum localhost:60001` to `wallet sync`, but
`--electrum tcp://localhost:60001` to `issuer transfer`. Reconciling these onto one
field is tracked for Phase 4.

> ⚠️ `electrum.blockstream.info:60002` is a **testnet** endpoint, not signet —
> Blockstream runs no public signet electrum. For signet use your own `electrs`
> (e.g. `tcp://127.0.0.1:60001`) or a trusted public signet electrum.

```bash
NET=signet
ELECTRUM_SYNC=127.0.0.1:60001            # bare host:port — for wallet sync
ELECTRUM_TX=tcp://127.0.0.1:60001        # scheme:// — for issuer transfer
DIR=~/.colorex
```

## 1. Create the issuer wallet

```bash
colorex wallet create \
  --network "$NET" --data-dir "$DIR/issuer" --name issuer \
  --account-file "$DIR/issuer.account"
```

This writes a fresh taproot (tapret) RGB wallet + an empty stock under
`$DIR/issuer/$NET`, plus an encrypted signing-account file at
`$DIR/issuer.account`. It prints a **keychain-10** (RGB anchor) receive address.

## 2. Fund + sync

Send coins to the printed keychain-10 address (faucet on a test net, real coins on
mainnet), wait for a confirmation, then sync:

```bash
colorex wallet sync \
  --network "$NET" --data-dir "$DIR/issuer" --name issuer \
  --electrum "$ELECTRUM_SYNC"
```

Issuance needs a confirmed **keychain-10** UTXO to anchor the genesis seal — step
3 auto-picks one from the synced wallet.

## 3. Mint the asset (NIA)

```bash
colorex issuer issue \
  --network "$NET" --data-dir "$DIR/issuer" --name issuer \
  --ticker FOO --asset-name "Foo Token" --precision 2 --supply 1000000
```

- `--supply` is in the **smallest unit** (so with `--precision 2`, `1000000` =
  10,000.00 FOO). The entire supply is allocated to the issuer at genesis — NIA
  is fixed-supply, minted once. Re-minting later needs an inflatable schema
  (deferred).
- Optional: `--details "..."`, `--seal <txid:vout>` to pin the genesis UTXO
  (otherwise auto-picked), `--issuer ssi:<label>` for the genesis identity.

It prints the new `rgb:...` **contract id**. Register it on a maker with
`colorex maker contract import rgb:...` — the assets a maker trades live in a
registry (`maker.db`), not the TOML (see [running-a-maker.md](running-a-maker.md) §3);
a taker passes the id to `colorex-taker`. Confirm the issued contract with:

```bash
colorex issuer contracts --network "$NET" --data-dir "$DIR/issuer" --name issuer
```

## 4. Distribute to a recipient

The recipient first creates an RGB invoice from their own wallet. Then the issuer
transfers against that invoice (note the **scheme** electrum URL here):

```bash
colorex issuer transfer \
  --network "$NET" --data-dir "$DIR/issuer" --name issuer \
  --electrum "$ELECTRUM_TX" \
  --account-file "$DIR/issuer.account" \
  --invoice <recipient-rgb-invoice> \
  --fee 1000
```

This signs + broadcasts the anchoring tx and prints a **consignment**. Hand the
consignment to the recipient; they accept it once the tx confirms, after which the
tokens show up in their inventory.

> **Maker recipients:** a maker can also just **be the issuer** — run the steps
> above pointed at the maker's own `--data-dir`/`--name` so the supply mints
> straight into the maker's wallet, skipping the invoice + transfer round-trip.
> See [running-a-maker.md](running-a-maker.md).

## Notes

- **Keychain layout:** external `0` (BTC), internal `1` (change), tapret `10`
  (RGB anchors). `wallet address --btc` prints a keychain-0 BTC address; the
  default prints the keychain-10 RGB one.
- **Other roles, same commands:** a maker/taker wallet is created the same way
  (`wallet create` with a different `--name`/`--data-dir`). `colorex maker init`
  additionally auto-creates the maker's wallet + account in one shot.
