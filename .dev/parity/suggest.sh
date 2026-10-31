#!/usr/bin/env bash
# Print the Pomb element that overlaps each golden element of a manifest entry.
# flags: --state --entry --manifest
set -euo pipefail

exec node "$(dirname "${BASH_SOURCE[0]}")/suggest.mjs" "$@"
