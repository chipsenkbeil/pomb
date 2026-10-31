#!/usr/bin/env bash
# Replay the parity manifest on a running Pomb and compare it with the goldens.
# flags: --state --manifest --out --only --debug-dir
set -euo pipefail

exec node "$(dirname "${BASH_SOURCE[0]}")/run.mjs" "$@"
