#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLS_DIR="$ROOT_DIR/tools"
ARTIFACTS_DIR="$ROOT_DIR/artifacts"
WALLETS_DIR="$ROOT_DIR/wallets"
DATA_DIR="$ROOT_DIR/data"
# Git-tracked NIA contract template. Lives outside ARTIFACTS_DIR (which
# regtest-reset wipes) so a reset stays re-bootstrappable.
NIA_TEMPLATE="${NIA_TEMPLATE:-$ROOT_DIR/contracts/rfq-nia.yaml}"

BITCOIN_WALLET="${BITCOIN_WALLET:-miner}"
ELECTRUM_URL="${ELECTRUM_URL:-localhost:60001}"
RGB_CMD_VERSION="${RGB_CMD_VERSION:-0.11.1-rc.6}"
BP_WALLET_VERSION="${BP_WALLET_VERSION:-0.11.1-alpha.2}"
SCHEMATA_DIR="${SCHEMATA_DIR:-$TOOLS_DIR/rgb-schemas/schemata}"
NIA_SCHEMA_FILE="${NIA_SCHEMA_FILE:-$SCHEMATA_DIR/NonInflatableAsset.rgb}"

compose() {
  (cd "$ROOT_DIR" && docker compose "$@")
}

bcli() {
  # `docker compose exec` runs bitcoin-cli as root inside the bitcoind container,
  # but bitcoind writes the cookie under /home/bitcoin/.bitcoin (it runs as the
  # `bitcoin` user, uid 101). Pointing -datadir at that path lets bitcoin-cli
  # read the cookie regardless of which user is invoking it.
  compose exec -T bitcoind bitcoin-cli -regtest -datadir=/home/bitcoin/.bitcoin "$@"
}

require_command() {
  local command_name="$1"
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "missing required command: $command_name" >&2
    exit 1
  fi
}

require_docker_services() {
  require_command docker
  if ! compose ps --status running bitcoind >/dev/null 2>&1; then
    echo "bitcoind is not running; start it with: make -C infra/regtest regtest-up" >&2
    exit 1
  fi
}

require_rgb_tools() {
  if [[ ! -x "$TOOLS_DIR/rgb-cmd/bin/rgb" ]]; then
    echo "rgb-cmd is not installed; run: make -C infra/regtest rgb-tools-install" >&2
    exit 1
  fi
  if [[ ! -x "$TOOLS_DIR/bp-wallet/bin/bp" || ! -x "$TOOLS_DIR/bp-wallet/bin/bp-hot" ]]; then
    echo "bp-wallet is not installed; run: make -C infra/regtest rgb-tools-install" >&2
    exit 1
  fi
}

require_rgb_schemas() {
  if [[ ! -f "$NIA_SCHEMA_FILE" ]]; then
    echo "rgb-schemas not available at $SCHEMATA_DIR; run: make -C infra/regtest rgb-schemas-fetch" >&2
    exit 1
  fi
}

ensure_dirs() {
  mkdir -p "$TOOLS_DIR" "$ARTIFACTS_DIR" "$WALLETS_DIR" "$DATA_DIR"/{issuer,maker,taker}
}

rgb_issuer() {
  "$TOOLS_DIR/rgb-cmd/bin/rgb" -n regtest --electrum="$ELECTRUM_URL" -d "$DATA_DIR/issuer" -w issuer "$@"
}

rgb_maker() {
  "$TOOLS_DIR/rgb-cmd/bin/rgb" -n regtest --electrum="$ELECTRUM_URL" -d "$DATA_DIR/maker" -w maker "$@"
}

rgb_taker() {
  "$TOOLS_DIR/rgb-cmd/bin/rgb" -n regtest --electrum="$ELECTRUM_URL" -d "$DATA_DIR/taker" -w taker "$@"
}

# Hot-sign a PSBT with a role's account file, then finalize + broadcast it.
# `rgb transfer` produces an UNSIGNED PSBT; without this the anchoring tx never
# hits the chain, leaving the RGB allocation on a never-mined (tentative)
# witness that vanishes on any witness re-resolution. `-N` = empty password
# (the bootstrap accounts are created password-less).
#   sign_and_publish <sender_rgb_wrapper> <role> <psbt>
sign_and_publish() {
  local rgb_wrapper="$1" role="$2" psbt="$3"
  "$TOOLS_DIR/bp-wallet/bin/bp-hot" sign "$psbt" "$WALLETS_DIR/$role.account" -N
  "$rgb_wrapper" finalize "$psbt" -p
}

manual_step() {
  local target="$1"
  shift
  cat >&2 <<MSG
$target is intentionally command-guided for now.
Reason: RGB CLI output values such as descriptors, outpoints, invoices, contract IDs, PSBT paths, and consignments need to be inspected and copied into the next step.

Use the manual checklist in docs/regtest-rgb20-nia-dev-infra.md.

Context:
$*
MSG
}
