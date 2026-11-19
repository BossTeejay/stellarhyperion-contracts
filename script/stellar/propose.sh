#!/usr/bin/env bash
#
# Emit unsigned, reviewed configuration proposals from a desired deployment manifest.
#
# Read only. Every chain access is a simulated view call; no source account signs anything and no
# transaction is submitted. The JSON on stdout is a handover artifact for the reviewer: queue those
# actions with the existing admin flow only after reading them and the recorded delay requirements.
#
# Usage:
#   script/stellar/propose.sh                                  # canonical active manifest
#   script/stellar/propose.sh path/to/desired-manifest.json
#   script/stellar/propose.sh --out /tmp/proposals.json
#   script/stellar/propose.sh --fixture script/stellar/test/fixtures/drift  # local rehearsal
#
# Fixture mode needs no Stellar CLI. Normal mode needs `stellar` on PATH; the source account named
# by STELLAR_IDENTITY (default hyperion-deploy) is used only to simulate reads, never to sign.

set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

command -v node >/dev/null || die "node is not on PATH"

if [[ " $* " != *" --fixture "* && " $* " != *" --fixture="* ]]; then
  command -v stellar >/dev/null || die "stellar is not on PATH; use --fixture for an offline rehearsal"
fi

exec node "$(dirname "${BASH_SOURCE[0]}")/propose.mjs" "$@"
