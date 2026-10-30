#!/usr/bin/env bash
# hidden
# Remove the files a verifier planted in the nightly backups directory: dev site decoys
# local
set -euo pipefail

# shellcheck source-path=SCRIPTDIR source=__common.sh
source "$(dirname "$0")/__common.sh"

pomb_ssh bash -s <<'REMOTE'
set -euo pipefail
dir=/var/lib/pomb/nightly/backups
for name in notes.txt pomb-20200101T000000Z-before-x.redb.partial pomb-20200101T000000Z.redb; do
  if [[ -f "$dir/$name" && ! -s "$dir/$name" ]]; then
    rm -f "$dir/$name"
    echo "removed $name"
  fi
done
REMOTE
