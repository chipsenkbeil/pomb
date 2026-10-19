#!/usr/bin/env bash
# Land a verified commit on main as one release commit and print the homelab request.
# flags: --batch --summary --env --dry-run
#
#   dev release <verified-commit> --batch 48 --summary "limits and docs"
#   dev release <verified-commit> --batch 48 --summary "..." --env KEY=VALUE
#   dev release <verified-commit> --batch 48 --summary "..." --dry-run
#
# Steps, any failure aborts before the push:
#   1. Refuse unless origin/main is the verified commit's base: an ancestor of it,
#      or sharing its merge-base tree, so applying the diff cannot revert main.
#   2. Apply the diff onto origin/main in .dev/worktrees/release-<N> and commit
#      "Release batch-<N>: <summary>".
#   3. Run dev lint, dev test and dev build there, logging to a temp dir.
#   4. Refuse attribution trailers in the new commit, then push to origin main.
#   5. Compute the nix hash of the GitHub archive and print the request block.
#
# --dry-run does everything except the push and the hash. The release worktree
# stays behind; dev verify-prod --cleanup removes it once the release is live.
# Repeat --env to list chipflow.service environment changes in the request.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
main_repo="$(cd "$root" && cd "$(git rev-parse --git-common-dir)/.." && pwd)"
tools="$root/.dev"

commit="" batch="" summary="" dry_run=0
envs=()

die() {
    printf 'release: ABORT: %s\n' "$*" >&2
    exit 1
}

usage() {
    echo 'usage: dev release <verified-commit> --batch N --summary "<text>" [--env KEY=VALUE]... [--dry-run]' >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
    --batch) batch="${2:-}"; shift ;;
    --batch=*) batch="${1#--batch=}" ;;
    --summary) summary="${2:-}"; shift ;;
    --summary=*) summary="${1#--summary=}" ;;
    --env) envs+=("${2:-}"); shift ;;
    --env=*) envs+=("${1#--env=}") ;;
    --dry-run) dry_run=1 ;;
    -*) usage ;;
    *)
        [ -z "$commit" ] || usage
        commit="$1"
        ;;
    esac
    shift
done
[ -n "$commit" ] && [ -n "$batch" ] && [ -n "$summary" ] || usage
case "$batch" in *[!0-9]* | "") die "--batch must be a number, got '$batch'" ;; esac
for kv in "${envs[@]+"${envs[@]}"}"; do
    case "$kv" in [A-Za-z_]*=*) ;; *) die "--env wants KEY=VALUE, got '$kv'" ;; esac
done

cd "$main_repo"
git fetch -q origin
cand=$(git rev-parse --verify "$commit^{commit}") || die "no such commit: $commit"
base=$(git rev-parse origin/main)

# 1. origin/main must be the commit's base
if ! git merge-base --is-ancestor "$base" "$cand"; then
    fork=$(git merge-base "$base" "$cand") || die "$commit shares no history with origin/main"
    [ "$(git rev-parse "$fork^{tree}")" = "$(git rev-parse "$base^{tree}")" ] ||
        die "origin/main (${base:0:7}) moved past the verified base (${fork:0:7}); re-verify on the new main"
fi

# 2. apply the diff in a fresh worktree and commit
wt=".dev/worktrees/release-$batch"
[ ! -e "$wt" ] || die "$wt already exists; remove it first"
git worktree add -q --detach "$wt" origin/main
cd "$wt"
git diff --binary origin/main "$cand" | git apply --index || die "diff does not apply (worktree left at $main_repo/$wt)"
git diff --quiet "$cand" || die "applied tree differs from $cand (worktree left at $main_repo/$wt)"
git diff --cached --quiet && die "verified commit changes nothing against origin/main"
git commit -q -m "Release batch-$batch: $summary"
sha=$(git rev-parse HEAD)

# 3. gates, in this worktree's own target dir so builds never reuse another tree's assets
logs=$(mktemp -d "${TMPDIR:-/tmp}/release-$batch.XXXXXX")
printf 'release: logs in %s\n' "$logs"
export CARGO_TARGET_DIR="$PWD/target"
for gate in lint test build; do
    printf 'release: dev %s ...\n' "$gate"
    dev "$gate" >"$logs/$gate.log" 2>&1 ||
        die "dev $gate failed, see $logs/$gate.log (worktree left at $main_repo/$wt)"
done
printf 'release: gates ok: %s\n' "$(rg -o 'Summary.*' "$logs/test.log" | tail -1)"

# 4. attribution check, then push
"$tools/__push_main.sh" "$PWD" --check || die "attribution check failed (worktree left at $main_repo/$wt)"
if [ "$dry_run" -eq 1 ]; then
    printf 'release: dry run, not pushed. Release commit %s in %s/%s\n' "$sha" "$main_repo" "$wt"
    exit 0
fi
"$tools/__push_main.sh" "$PWD" || die "push failed (worktree left at $main_repo/$wt)"

# 5. nix hash and the request
hash=$(nix --extra-experimental-features 'nix-command flakes' store prefetch-file --json --unpack \
    "https://github.com/chipsenkbeil/chipflow/archive/$sha.tar.gz" 2>/dev/null | jq -r .hash || true)
[ -n "$hash" ] && [ "$hash" != null ] || die "pushed $sha but the nix hash failed; rerun the prefetch by hand"

printf '\n===== homelab request =====\n'
printf 'ChipFlow sha: %s\n' "$sha"
printf 'nix hash:     %s\n' "$hash"
printf 'batch:        %s\n' "$batch"
if [ "${#envs[@]}" -gt 0 ]; then
    printf 'chipflow.service env changes:\n'
    printf '  %s\n' "${envs[@]}"
fi
printf 'condition:    dry run must show only chipflow.service\n'
printf '===========================\n'
