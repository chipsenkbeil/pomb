#!/usr/bin/env bash
# Check the task finder of a candidate server against its state file.
# flags: --candidate
set -euo pipefail

exec node "$(dirname "${BASH_SOURCE[0]}")/finder.mjs" "$@"
