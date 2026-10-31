#!/usr/bin/env bash
# Hidden helper: run one dev gate on ci and fail unless ci tested the worktree.
set -euo pipefail

wt="${1:?usage: __gate.sh <worktree> <verb> <log>}"
verb="${2:?usage: __gate.sh <worktree> <verb> <log>}"
log="${3:?usage: __gate.sh <worktree> <verb> <log>}"

runs() {
    ci -C "$wt" --json ls -n 200 | jq -c '.runs[]'
}

before=$(runs | jq -r .run | sort)
(cd "$wt" && dev "$verb" --remote) >"$log" 2>&1 || {
    printf '%s: dev %s --remote failed\n' "$wt" "$verb" >&2
    exit 1
}
after=$(runs)

new=$(comm -13 <(printf '%s\n' "$before") <(printf '%s\n' "$after" | jq -r .run | sort) | sed '/^$/d')
count=$(printf '%s\n' "$new" | sed '/^$/d' | wc -l | tr -d ' ')
if [ "$count" != 1 ]; then
    printf 'dev %s --remote left %s new ci runs under %s, expected 1: it did not test this worktree\n' "$verb" "$count" "$wt" >&2
    exit 1
fi

row=$(printf '%s\n' "$after" | jq -c --arg id "$new" 'select(.run == $id)')
status=$(jq -r .status <<<"$row")
source=$(jq -r .source <<<"$row")
repo=$(jq -r .repo <<<"$row")
if [ "$status" != passed ] || [ "$source" != local ]; then
    printf 'ci run %s of %s is %s from %s, expected passed from local\n' "$new" "$repo" "$status" "$source" >&2
    exit 1
fi
printf '%s %s %s\n' "$new" "$repo" "$source"
