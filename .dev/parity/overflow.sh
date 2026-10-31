#!/usr/bin/env bash
# Check that the candidate pages do not scroll sideways at a phone width.
set -euo pipefail

exec node "$(dirname "${BASH_SOURCE[0]}")/overflow.mjs" "$@"
