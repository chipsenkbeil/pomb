#!/usr/bin/env bash
# Hidden helper: refuse a release whose version was not bumped past the base or whose Cargo.lock disagrees.
set -euo pipefail

usage='usage: __version_guard.sh <base Cargo.toml> <candidate Cargo.toml> <candidate Cargo.lock>'
base_toml="${1:?$usage}"
cand_toml="${2:?$usage}"
cand_lock="${3:?$usage}"

manifest_version() {
    awk '
        /^\[/ { in_package = ($0 == "[package]") }
        in_package && /^version = "/ { gsub(/^version = "|"$/, ""); print; exit }' "$1"
}

lock_version() {
    awk '
        $0 == "name = \"pomb\"" { found = 1; next }
        found && /^version = "/ { gsub(/^version = "|"$/, ""); print; exit }' "$1"
}

new=$(manifest_version "$cand_toml")
old=$(manifest_version "$base_toml")
locked=$(lock_version "$cand_lock")
[ -n "$new" ] || { printf 'no package version in the candidate Cargo.toml\n' >&2; exit 1; }
[ -n "$old" ] || { printf 'no package version in the base Cargo.toml\n' >&2; exit 1; }

if [ "$new" = "$old" ]; then
    printf 'Cargo.toml version %s equals the base version %s; bump and commit it\n' "$new" "$old" >&2
    exit 1
fi
if [ "$locked" != "$new" ]; then
    printf 'Cargo.lock pomb version %s differs from Cargo.toml version %s; commit the lock update\n' "${locked:-none}" "$new" >&2
    exit 1
fi
