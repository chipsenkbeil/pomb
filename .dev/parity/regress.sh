#!/usr/bin/env bash
# Diff the screenshots of two running Pomb builds entry by entry.
# flags: --candidate --baseline --out --manifest --only --max-diff --fuzz
set -euo pipefail

exec node "$(dirname "${BASH_SOURCE[0]}")/regress.mjs" "$@"
