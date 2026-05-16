SHELL := /usr/bin/env bash

.PHONY: regtest-up regtest-down regtest-reset regtest-mine rgb-tools-install rgb-schemas-fetch rgb-wallets-init rgb-create-wallets rgb-fund-wallets rgb-issue-asset rgb-transfer-maker rgb-transfer-taker regtest-bootstrap check

regtest-up:
	./scripts/regtest-up

regtest-down:
	./scripts/regtest-down

regtest-reset:
	./scripts/regtest-reset

regtest-mine:
	./scripts/regtest-mine $(BLOCKS)

rgb-tools-install:
	./scripts/rgb-tools-install

rgb-schemas-fetch:
	./scripts/rgb-schemas-fetch

rgb-wallets-init:
	./scripts/rgb-wallets-init

rgb-create-wallets:
	./scripts/rgb-create-wallets

rgb-fund-wallets:
	./scripts/rgb-fund-wallets

rgb-issue-asset:
	./scripts/rgb-issue-asset

rgb-transfer-maker:
	./scripts/rgb-transfer-maker

rgb-transfer-taker:
	./scripts/rgb-transfer-taker

## Full clean → ready-to-test bootstrap. Runs every step in order; each
## script is idempotent so re-running picks up where a previous attempt
## left off. Set SKIP_TRANSFER=1 to stop after issuing the contract.
regtest-bootstrap:
	$(MAKE) regtest-up
	$(MAKE) rgb-tools-install
	$(MAKE) rgb-schemas-fetch
	./scripts/regtest-bootstrap-mine-initial
	$(MAKE) rgb-wallets-init
	$(MAKE) rgb-create-wallets
	$(MAKE) rgb-fund-wallets
	$(MAKE) rgb-issue-asset
	@if [ "$${SKIP_TRANSFER:-0}" != "1" ]; then $(MAKE) rgb-transfer-maker; fi
	./scripts/regtest-bootstrap-print-env

check:
	docker compose config >/dev/null
	bash -n scripts/*
