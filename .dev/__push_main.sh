#!/usr/bin/env bash
# Hidden helper: refuse attribution trailers, then push HEAD to origin main.
#
#   .dev/__push_main.sh <worktree>             check, then push
#   .dev/__push_main.sh <worktree> --check     check only
#
# dev push pushes the branch HEAD names, and a detached release worktree has
# none, so the push goes through jj-git-guard, the same attribution guard.
set -euo pipefail

wt="${1:?usage: __push_main.sh <worktree> [--check]}"
mode="${2:-push}"

hits=$(git -C "$wt" log --format=%B origin/main..HEAD |
    rg -i 'Claude-Session|Co-Authored-By|Generated with|claude\.ai/code' || true)
if [ -n "$hits" ]; then
    printf 'attribution found in commit messages:\n%s\n' "$hits" >&2
    exit 1
fi
[ "$mode" = "--check" ] && exit 0

sha=$(git -C "$wt" rev-parse HEAD)
cd "$wt"
jj-git-guard push -- origin "$sha:refs/heads/main"
