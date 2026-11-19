#!/usr/bin/env bash
#
# Local upgrade rehearsal fixture runner for HyperionRouter (#53).
#
# Snapshots router storage state, simulates a WASM upgrade over persisted ledger entries,
# and verifies every critical record through public contract methods. Also runs the
# storage cost projection tool against a representative fixture footprint.
#
# Usage: script/stellar/rehearse-upgrade.sh [--build-wasm]
source "$(dirname "$0")/lib.sh"

BUILD_WASM=0
for arg in "$@"; do
  case "$arg" in
    --build-wasm) BUILD_WASM=1 ;;
    *) die "unknown option: $arg" ;;
  esac
done

if [ "$BUILD_WASM" -eq 1 ]; then
  note "building router WASM"
  "$CONTRACTS_ROOT/script/stellar/build.sh"
fi

note "running Soroban upgrade rehearsal snapshot & comparison fixture"
cargo test --manifest-path "$SOROBAN_DIR/Cargo.toml" \
  -p hyperion-router \
  storage_maintenance::upgrade_rehearsal_snapshots_and_compares_critical_records_via_public_methods

note "running storage cost projection against rehearsal fixture"
node "$CONTRACTS_ROOT/script/stellar/storage-cost.mjs" \
  --out-nonce 100 \
  --claim-count 10 \
  --archived-claims 4 \
  --queue-count 2 \
  --processed-count 10

printf '\n\033[32mUpgrade rehearsal and storage projection passed.\033[0m\n'
