#!/usr/bin/env bash
# Land a verified commit on main as one release commit and print the homelab request.
# flags: --batch --summary --env --dry-run --hash-only
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
#   3. Run dev lint --ci and dev test --ci (in parallel, on ci) and a local dev build
#      there, logging to a temp dir. A failing gate aborts before the push.
#   4. Refuse attribution trailers in the new commit, then push to origin main.
#   5. Compute the nix hash of the GitHub archive (300 second limit, three attempts)
#      and print the request block.
#
# --hash-only <sha> prints the request block for an already pushed sha and touches
# no git state: the recovery path when the hash step fails after the push.
#
# --dry-run does everything except the push and the hash. The release worktree
# stays behind; dev verify-prod --cleanup removes it once the release is live.
# Repeat --env to list pomb.service environment changes in the request.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
main_repo="$(cd "$root" && cd "$(git rev-parse --git-common-dir)/.." && pwd)"
tools="$root/.dev"

commit="" batch="" summary="" dry_run=0 hash_only=""
envs=()

die() {
    printf 'release: ABORT: %s\n' "$*" >&2
    exit 1
}

usage() {
    echo 'usage: dev release <verified-commit> --batch N --summary "<text>" [--env KEY=VALUE]... [--dry-run]' >&2
    echo '       dev release --hash-only <sha> [--batch N] [--env KEY=VALUE]...' >&2
    exit 2
}

# bounded <seconds> <cmd>...: run cmd, killing it after the limit. Uses timeout or gtimeout
# when present (macOS has neither without coreutils), else a background watchdog on the exact pid.
bounded() {
    local secs=$1 t pid dog rc=0
    shift
    if t=$(command -v timeout || command -v gtimeout); then
        "$t" "$secs" "$@"
        return
    fi
    "$@" &
    pid=$!
    (
        sleep "$secs"
        kill "$pid" 2>/dev/null
    ) &
    dog=$!
    wait "$pid" || rc=$?
    pkill -P "$dog" 2>/dev/null || true
    kill "$dog" 2>/dev/null || true
    wait "$dog" 2>/dev/null || true
    return "$rc"
}

# nix_hash <sha>: print the sri hash of the GitHub archive; 300 seconds per try, three tries.
nix_hash() {
    local sha=$1 out try
    for try in 1 2 3; do
        out=$(bounded 300 nix --extra-experimental-features 'nix-command flakes' store prefetch-file --json --unpack \
            "https://github.com/chipsenkbeil/pomb/archive/$sha.tar.gz" 2>/dev/null | jq -r .hash) || out=""
        if [ -n "$out" ] && [ "$out" != null ]; then
            printf '%s\n' "$out"
            return 0
        fi
        printf 'release: nix hash attempt %s of 3 failed\n' "$try" >&2
    done
    return 1
}

# request_block <sha> <hash>
request_block() {
    printf '\n===== homelab request =====\n'
    printf 'Pomb sha: %s\n' "$1"
    printf 'nix hash:     %s\n' "$2"
    [ -z "$batch" ] || printf 'batch:        %s\n' "$batch"
    if [ "${#envs[@]}" -gt 0 ]; then
        printf 'pomb.service env changes:\n'
        printf '  %s\n' "${envs[@]}"
    fi
    printf 'condition:    dry run must show only pomb.service\n'
    printf '===========================\n'
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
    --hash-only) hash_only="${2:-}"; shift ;;
    --hash-only=*) hash_only="${1#--hash-only=}" ;;
    -*) usage ;;
    *)
        [ -z "$commit" ] || usage
        commit="$1"
        ;;
    esac
    shift
done
if [ -n "$hash_only" ]; then
    [ -z "$commit" ] && [ -z "$summary" ] || usage
else
    [ -n "$commit" ] && [ -n "$batch" ] && [ -n "$summary" ] || usage
fi
case "$batch" in *[!0-9]*) die "--batch must be a number, got '$batch'" ;; esac
for kv in "${envs[@]+"${envs[@]}"}"; do
    case "$kv" in [A-Za-z_]*=*) ;; *) die "--env wants KEY=VALUE, got '$kv'" ;; esac
done

if [ -n "$hash_only" ]; then
    [[ "$hash_only" =~ ^[0-9a-f]{40}$ ]] || die "--hash-only wants a full 40 character sha"
    hash=$(nix_hash "$hash_only") ||
        die "nix hash failed for $hash_only; rerun: dev release --hash-only $hash_only"
    request_block "$hash_only" "$hash"
    exit 0
fi

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
printf 'release: dev lint --ci and dev test --ci ...\n'
dev lint --ci >"$logs/lint.log" 2>&1 &
lint_pid=$!
dev test --ci >"$logs/test.log" 2>&1 &
test_pid=$!
lint_rc=0 test_rc=0
wait "$lint_pid" || lint_rc=$?
wait "$test_pid" || test_rc=$?
[ "$lint_rc" -eq 0 ] || die "dev lint --ci failed, see $logs/lint.log (worktree left at $main_repo/$wt)"
[ "$test_rc" -eq 0 ] || die "dev test --ci failed, see $logs/test.log (worktree left at $main_repo/$wt)"
printf 'release: dev build ...\n'
dev build >"$logs/build.log" 2>&1 ||
    die "dev build failed, see $logs/build.log (worktree left at $main_repo/$wt)"
printf 'release: gates ok: %s\n' "$(rg -o 'Summary.*' "$logs/test.log" | tail -1)"

# 4. attribution check, then push
"$tools/__push_main.sh" "$PWD" --check || die "attribution check failed (worktree left at $main_repo/$wt)"
if [ "$dry_run" -eq 1 ]; then
    printf 'release: dry run, not pushed. Release commit %s in %s/%s\n' "$sha" "$main_repo" "$wt"
    exit 0
fi
"$tools/__push_main.sh" "$PWD" || die "push failed (worktree left at $main_repo/$wt)"

# 5. nix hash and the request
hash=$(nix_hash "$sha") ||
    die "pushed $sha but the nix hash failed; run: dev release --hash-only $sha"
request_block "$sha" "$hash"
