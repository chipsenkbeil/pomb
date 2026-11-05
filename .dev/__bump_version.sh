#!/usr/bin/env bash
set -euo pipefail

usage='usage: __bump_version.sh <dir holding Cargo.toml and Cargo.lock>'
dir="${1:?$usage}"
manifest="$dir/Cargo.toml"
lock="$dir/Cargo.lock"

manifest_line() {
    awk '
        /^\[/ { in_package = ($0 == "[package]") }
        in_package && /^version = "/ { print NR; exit }' "$1"
}

lock_line() {
    awk '
        /^\[\[package\]\]/ { found = 0 }
        $0 == "name = \"pomb\"" { found = 1; next }
        found && /^version = "/ { print NR; exit }' "$1"
}

version_at() {
    sed -n "$2p" "$1" | cut -d'"' -f2
}

set_version_at() {
    local tmp
    tmp=$(mktemp)
    sed "$2s/\"[^\"]*\"/\"$3\"/" "$1" >"$tmp"
    cat "$tmp" >"$1"
    rm -f "$tmp"
}

manifest_ln=$(manifest_line "$manifest")
[ -n "$manifest_ln" ] || { printf 'no package version in %s\n' "$manifest" >&2; exit 1; }
lock_ln=$(lock_line "$lock")
[ -n "$lock_ln" ] || { printf 'no pomb entry in %s\n' "$lock" >&2; exit 1; }

old=$(version_at "$manifest" "$manifest_ln")
if ! [[ "$old" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
    printf 'version %s in %s is not MAJOR.MINOR.PATCH\n' "$old" "$manifest" >&2
    exit 1
fi
new="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.$((BASH_REMATCH[3] + 1))"

set_version_at "$manifest" "$manifest_ln" "$new"
set_version_at "$lock" "$lock_ln" "$new"
printf '%s\n' "$new"
