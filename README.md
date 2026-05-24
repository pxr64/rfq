# RGB RFQ

Minimal Rust workspace for an RGB-ready RFQ architecture. The code is intentionally scaffolded: it defines crate boundaries, traits, mocked components, and an in-memory Axum flow without real RGB, BDK, Lightning, persistence, or matching logic.

## Crates

- `rfq-types`: shared dependency-light DTOs and domain types; depends only on `serde`.
- `rfq-core`: request validation, quote expiry, and quote sorting helpers.
- `rfq-router`: async maker connector trait and concurrent RFQ fanout.
- `rfq-rgb`: RGB backend trait and mock adapter layer; no real RGB libraries yet.
- `rfq-store`: in-memory quote storage.
- `rfq-maker`: mock maker implementation with fixed pricing and mock settlement.
- `rfq-api`: Axum HTTP API exposing RFQ and quote acceptance endpoints.
- `rfq-client`: thin public HTTP SDK over `rfq-api`.
- `rfq-wallet`: browser-neutral wallet traits and mock wallet backend.
- `maker-node`: placeholder maker daemon CLI.
- `wallet-wasm`: wasm-bindgen wrapper around mocked wallet functions.

## Boundaries

- `rfq-types` stays dependency-free except `serde`.
- `rfq-core` does not depend on wallet, RGB, API, or node crates.
- `rfq-api` does not depend on `rfq-wallet`.
- `rfq-wallet` has no browser-specific code and no RGB/BDK integrations.

## Commands

```bash
cargo check --workspace
cargo test --workspace
cargo run -p rfq-api
cargo run -p maker-node --bin colorex -- maker health
```

## Regtest RGB Infra

The first on-chain RGB20/NIA dev stack lives under `infra/regtest`.

```bash
make -C infra/regtest regtest-up
make -C infra/regtest regtest-mine BLOCKS=103
make -C infra/regtest rgb-tools-install
```

See `docs/regtest-rgb20-nia-dev-infra.md` for the manual issue and transfer checklist.
# rfq
