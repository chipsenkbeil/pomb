#!/usr/bin/env bash
# Enable and start a channel's service and update timer: dev site launch <stable|nightly>
# local
set -euo pipefail

# shellcheck source-path=SCRIPTDIR source=__common.sh
source "$(dirname "$0")/__common.sh"

channel="${1:-}"
case "$channel" in
  stable | nightly) ;;
  *)
    echo "usage: dev site launch <stable|nightly>" >&2
    exit 2
    ;;
esac

pomb_ssh systemctl enable --now "pomb-$channel.service" "pomb-update@$channel.timer"
pomb_ssh systemctl is-active "pomb-$channel.service" "pomb-update@$channel.timer"
