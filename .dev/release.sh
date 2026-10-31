#!/usr/bin/env bash
# Land a verified commit on main as one release commit and print the homelab request.
# flags: --batch --summary --env --dry-run --hash-only
#
#   dev release <verified-commit> --batch 48 --summary "limits and docs"
#   dev release <verified-commit> --batch 48 --summary "..." --env KEY=VALUE
#   dev release <verified-commit> --batch 48 --summary "..." --dry-run
#
# Steps, any failure aborts before the push:
#   1. Refuse unless <remote>/main is the verified commit's base: an ancestor of it,
#      or sharing its merge-base tree, so applying the diff cannot revert main.
#   2. Apply the diff onto <remote>/main in .dev/worktrees/release-<N> and commit
#      "Release batch-<N>: <summary>".
#   3. Run dev lint --remote, dev test --remote and dev build --remote, one at a time on ci
#      there, logging to a temp dir. A failing gate aborts before the push.
#   4. Refuse attribution trailers in the new commit, then push to main on the remote of record.
#   5. When the remote of record is gerrit, wait for the GitHub mirror to hold the sha.
#      Then compute the nix hash of the GitHub archive (300 second limit, three attempts)
#      and print the request block.
#
# The remote of record is forge.remote, else gerrit when that remote exists, else origin.
# On gerrit the release commit carries a Change-Id trailer.
#
# --hash-only <sha> prints the request block for an already pushed sha and touches
# no git state, after the same mirror wait on gerrit: the recovery path when the
# hash step fails after the push.
#
# --dry-run does everything except the push and the hash. The release worktree
# stays behind. dev verify --cleanup removes it once the release is live.
# Repeat --env to list pomb.service environment changes in the request.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
main_repo="$(cd "$root" && cd "$(git rev-parse --git-common-dir)/.." && pwd)"
tools="$root/.dev"
mirror_wait_secs=900 mirror_step_secs=15

# shellcheck source=__remote.sh
. "$tools/__remote.sh"

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
    cd "$main_repo"
    remote=$(remote_of_record) || exit 1
    if [ "$remote" = gerrit ]; then
        wait_for_mirror "$hash_only" "$mirror_wait_secs" "$mirror_step_secs" || die "run again once the mirror has caught up"
    fi
    hash=$(nix_hash "$hash_only") ||
        die "nix hash failed for $hash_only; rerun: dev release --hash-only $hash_only"
    request_block "$hash_only" "$hash"
    exit 0
fi

cd "$main_repo"
remote=$(remote_of_record) || exit 1
git fetch -q "$remote"
cand=$(git rev-parse --verify "$commit^{commit}") || die "no such commit: $commit"
base=$(git rev-parse "$remote/main")

# 1. the remote's main must be the commit's base
if ! git merge-base --is-ancestor "$base" "$cand"; then
    fork=$(git merge-base "$base" "$cand") || die "$commit shares no history with $remote/main"
    [ "$(git rev-parse "$fork^{tree}")" = "$(git rev-parse "$base^{tree}")" ] ||
        die "$remote/main (${base:0:7}) moved past the verified base (${fork:0:7}); re-verify on the new main"
fi

# 2. apply the diff in a fresh worktree and commit
wt=".dev/worktrees/release-$batch"
[ ! -e "$wt" ] || die "$wt already exists; remove it first"
git worktree add -q --detach "$wt" "$remote/main"
cd "$wt"
git diff --binary "$remote/main" "$cand" | git apply --index || die "diff does not apply (worktree left at $main_repo/$wt)"
git diff --quiet "$cand" || die "applied tree differs from $cand (worktree left at $main_repo/$wt)"
git diff --cached --quiet && die "verified commit changes nothing against $remote/main"
git commit -q -m "Release batch-$batch: $summary"
[ "$remote" != gerrit ] || add_change_id
sha=$(git rev-parse HEAD)

# 3. gates, one at a time on ci
logs=$(mktemp -d "${TMPDIR:-/tmp}/release-$batch.XXXXXX")
printf 'release: logs in %s\n' "$logs"
export CARGO_TARGET_DIR="$PWD/target"
printf 'release: dev lint --remote ...\n'
dev lint --remote >"$logs/lint.log" 2>&1 ||
    die "dev lint --remote failed, see $logs/lint.log (worktree left at $main_repo/$wt)"
printf 'release: dev test --remote ...\n'
dev test --remote >"$logs/test.log" 2>&1 ||
    die "dev test --remote failed, see $logs/test.log (worktree left at $main_repo/$wt)"
printf 'release: dev build --remote ...\n'
dev build --remote >"$logs/build.log" 2>&1 ||
    die "dev build --remote failed, see $logs/build.log (worktree left at $main_repo/$wt)"
printf 'release: gates ok: %s\n' "$(rg -o 'Summary.*' "$logs/test.log" | tail -1)"

# 4. attribution check, then push
"$tools/__push_main.sh" "$PWD" --check || die "attribution check failed (worktree left at $main_repo/$wt)"
if [ "$dry_run" -eq 1 ]; then
    printf 'release: dry run, not pushed. Release commit %s in %s/%s\n' "$sha" "$main_repo" "$wt"
    exit 0
fi
"$tools/__push_main.sh" "$PWD" || die "push failed (worktree left at $main_repo/$wt)"

# 5. mirror wait, nix hash and the request
if [ "$remote" = gerrit ]; then
    wait_for_mirror "$sha" "$mirror_wait_secs" "$mirror_step_secs" ||
        die "pushed $sha to gerrit but the mirror is behind; run: dev release --hash-only $sha"
fi
hash=$(nix_hash "$sha") ||
    die "pushed $sha but the nix hash failed; run: dev release --hash-only $sha"
request_block "$sha" "$hash"
