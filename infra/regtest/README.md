# Regtest RGB Infra

Reproducible local infrastructure for the first on-chain RGB20/NIA RFQ MVP.

## Quick Start

```bash
make -C infra/regtest regtest-up
make -C infra/regtest regtest-mine BLOCKS=103
make -C infra/regtest rgb-tools-install
```

The manual RGB issue/transfer checklist is in `docs/regtest-rgb20-nia-dev-infra.md`.

## Targets

- `regtest-up`: start `bitcoind` and `electrs`. On first run, builds `electrs` from `romanz/electrs` v0.11.1 source via [electrs.Dockerfile](electrs.Dockerfile) (~5-10 min); subsequent runs use the cached image. Override the upstream ref via `docker compose build --build-arg ELECTRS_REF=<tag>`. Reason for the local build: bp-wallet 0.11.1-alpha.2 only parses romanz/electrs response shapes; blockstream-fork images (mempool/electrs, getumbrel/electrs) trip an internal `expect("broken logic")` panic.
- `regtest-down`: stop services
- `regtest-reset`: stop services, remove Docker volumes, and clear generated local RGB/wallet data
- `regtest-mine [BLOCKS=N]`: create/load the miner wallet and mine `N` blocks (default `1`)
- `rgb-tools-install`: install pinned `bp-wallet` and `rgb-cmd`
- `rgb-schemas-fetch`: clone `RGB-WG/rgb-schemata` into `tools/rgb-schemas/` (idempotent; override with `RGB_SCHEMAS_REPO=`/`RGB_SCHEMAS_REF=`)
- `rgb-wallets-init`: create bp-hot seeds and bip84 P2WPKH descriptors for issuer/maker/taker under `wallets/` (idempotent)
- `rgb-fund-wallets`: derive a keychain-9 address per role, send 1 BTC each from the miner wallet, mine a confirmation, and sync the three RGB wallets (amount overridable via `RGB_FUND_AMOUNT=`)
- `rgb-issue-asset`: validate prerequisites and point to the issuance checklist
- `rgb-transfer-maker`: validate prerequisites and point to the issuer-to-maker transfer checklist
- `rgb-transfer-taker`: validate prerequisites and point to the maker-to-taker transfer checklist
- `check`: parse Docker Compose config and shell-check script syntax with `bash -n`

## Generated Paths

The following paths are intentionally ignored by git:

- `infra/regtest/tools`
- `infra/regtest/data`
- `infra/regtest/wallets`
- `infra/regtest/artifacts`
- `infra/regtest/contracts/generated`
