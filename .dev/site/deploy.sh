#!/usr/bin/env bash
# Run the box's pull agent now: dev site deploy <stable|nightly> [<tag>]
# local
set -euo pipefail

# shellcheck source-path=SCRIPTDIR source=__common.sh
source "$(dirname "$0")/__common.sh"

channel="${1:-}"
tag="${2:-}"
case "$channel" in
  stable | nightly) ;;
  *)
    echo "usage: dev site deploy <stable|nightly> [<tag>]" >&2
    exit 2
    ;;
esac

pomb_ssh /usr/local/sbin/pomb-update "$channel" ${tag:+"$tag"}
