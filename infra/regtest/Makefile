SHELL := /usr/bin/env bash

.PHONY: regtest-up regtest-down regtest-reset regtest-mine rgb-tools-install rgb-schemas-fetch rgb-wallets-init rgb-fund-wallets rgb-issue-asset rgb-transfer-maker rgb-transfer-taker check

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

rgb-fund-wallets:
	./scripts/rgb-fund-wallets

rgb-issue-asset:
	./scripts/rgb-issue-asset

rgb-transfer-maker:
	./scripts/rgb-transfer-maker

rgb-transfer-taker:
	./scripts/rgb-transfer-taker

check:
	docker compose config >/dev/null
	bash -n scripts/*
