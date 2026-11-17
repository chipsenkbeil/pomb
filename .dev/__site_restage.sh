#!/usr/bin/env bash
set -euo pipefail
exec 9>/run/lock/pomb-update-nightly.lock
if ! flock -n 9; then
  echo "An update is running." >&2
  exit 1
fi
base=/opt/pomb/nightly
cur=$(readlink "$base/current") || {
  echo "There is no current release." >&2
  exit 1
}
id=$(basename "$cur")
if [[ "$id" == before-* ]]; then
  echo "The current release is already $id, so run dev site deploy nightly force to replace it." >&2
  exit 1
fi
if [[ -n "${BAD:-}" && ! -f /var/lib/pomb/nightly/pomb.redb ]]; then
  echo "The --bad option needs an existing pomb.redb." >&2
  exit 1
fi
if [[ -n "${BAD:-}" ]] && ! systemctl is-enabled --quiet pomb-nightly.service; then
  echo "The --bad option needs pomb-nightly.service enabled." >&2
  exit 1
fi
prev="$base/releases/before-$id"
rm -rf "$prev"
cp -a "$cur" "$prev"
ln -sfn "$prev" "$base/current.new"
mv -T "$base/current.new" "$base/current"
rm -rf "$cur"
if [[ -n "${BAD:-}" ]]; then
  mkdir "$cur"
  printf '#!/bin/sh\nexit 1\n' >"$cur/pomb"
  chmod 755 "$cur/pomb"
  cp "$prev/commit" "$cur/commit"
fi
echo "The nightly release is now $(basename "$(readlink "$base/current")")."
