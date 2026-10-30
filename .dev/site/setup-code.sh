#!/usr/bin/env bash
# Print the setup code each channel logged at its first start: dev site setup-code
# local
set -euo pipefail

# shellcheck source-path=SCRIPTDIR source=__common.sh
source "$(dirname "$0")/__common.sh"

pomb_ssh bash -s <<'REMOTE'
for ch in stable nightly; do
  printf '%-8s %s\n' "$ch" "$(journalctl -u "pomb-$ch" --no-pager -q | grep "setup code" | tail -n 1 | sed 's/.*setup code/setup code/')"
done
REMOTE
