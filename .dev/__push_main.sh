#!/usr/bin/env bash
# Hidden helper: refuse attribution trailers, then push HEAD to main on the remote of record.
#
#   .dev/__push_main.sh <worktree>             check, then push
#   .dev/__push_main.sh <worktree> --check     check only
#
# dev push pushes the branch HEAD names, and a detached release worktree has
# none, so the push goes through jj-git-guard, the same attribution guard.
set -euo pipefail

wt="${1:?usage: __push_main.sh <worktree> [--check]}"
mode="${2:-push}"

# shellcheck source=__remote.sh
. "$(dirname "${BASH_SOURCE[0]}")/__remote.sh"
remote=$(remote_of_record "$wt")

hits=$(git -C "$wt" log --format=%B "$remote/main..HEAD" |
    rg -i 'Claude-Session|Co-Authored-By|Generated with|claude\.ai/code' || true)
trailers=$(git -C "$wt" rev-list "$remote/main..HEAD" |
    while read -r c; do git -C "$wt" log -1 --format=%B "$c" | git interpret-trailers --parse; done |
    rg -v '^Change-Id: I[0-9a-f]{40}$' || true)
hits+=${hits:+$'\n'}$trailers
if [ -n "$hits" ]; then
    printf 'attribution or trailers found in commit messages:\n%s\n' "$hits" >&2
    exit 1
fi
[ "$mode" = "--check" ] && exit 0

sha=$(git -C "$wt" rev-parse HEAD)
cd "$wt"
log=$(mktemp)
trap 'rm -f "$log"' EXIT
status=0
jj-git-guard push -- "$remote" "$sha:refs/heads/main" 2>"$log" || status=$?
cat "$log" >&2
[ "$status" -eq 0 ] && exit 0
if rg -q 'non-fast-forward|fetch first' "$log"; then
    exit 3
fi
exit 1
