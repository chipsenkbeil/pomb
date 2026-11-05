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
#      Then refuse unless Cargo.toml's package version differs from <remote>/main's and
#      Cargo.lock's pomb entry matches it.
#   2. Apply the diff onto <remote>/main in .dev/worktrees/release-<N> and commit
#      "Release batch-<N>: <summary>".
#   3. Run dev lint --remote, dev test --remote, dev nixcheck --remote and dev build --remote,
#      one at a time on ci, logging to a temp dir. A failing gate aborts before the push, and
#      so does a gate whose ci run is not a new passed run of the release worktree with
#      source local.
#   4. Refuse attribution trailers in the new commit, then push to main on the remote of record.
#   5. Compute the nix hash of the commit's tree and print the request block.
#
# The remote of record is forge.remote, else gerrit when that remote exists, else origin.
# On gerrit the release commit carries a Change-Id trailer.
#
# --hash-only <sha> fetches main from the remote of record and prints the request block
# for an already pushed sha: the recovery path when the hash step fails after the push.
#
# --dry-run does everything except the push and the hash. The release worktree
# stays behind. dev verify --cleanup removes it once the release is live.
# Repeat --env to list pomb.service environment changes in the request.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
main_repo="$(cd "$root" && cd "$(git rev-parse --git-common-dir)/.." && pwd)"
tools="$root/.dev"

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

# nix_hash <sha>: print the sri hash of the commit's tree, the hash the homelab pin checks.
nix_hash() {
    local sha=$1 dir out=""
    dir=$(mktemp -d)
    if git -C "$main_repo" archive "$sha" | tar -x -C "$dir"; then
        out=$(nix --extra-experimental-features nix-command hash path "$dir") || out=""
    fi
    rm -rf "$dir"
    [ -n "$out" ] && printf '%s\n' "$out"
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
    git -C "$main_repo" fetch -q "$(remote_of_record "$main_repo")" main || die "fetch of main failed"
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

guard_dir=$(mktemp -d)
{
    git show "$remote/main:Cargo.toml" >"$guard_dir/base.toml" &&
        git show "$cand:Cargo.toml" >"$guard_dir/cand.toml" &&
        git show "$cand:Cargo.lock" >"$guard_dir/cand.lock"
} || { rm -rf "$guard_dir"; die "cannot read Cargo.toml or Cargo.lock at $commit"; }
guard_ok=0
"$tools/__version_guard.sh" "$guard_dir/base.toml" "$guard_dir/cand.toml" "$guard_dir/cand.lock" || guard_ok=1
rm -rf "$guard_dir"
[ "$guard_ok" = 0 ] || die "version check failed for $commit"

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
for gate in lint test nixcheck build; do
    printf 'release: dev %s --remote ...\n' "$gate"
    ran=$("$tools/__gate.sh" "$PWD" "$gate" "$logs/$gate.log" 2>"$logs/$gate.err") ||
        die "dev $gate --remote did not pass on this worktree: $(cat "$logs/$gate.err"), see $logs/$gate.log (worktree left at $main_repo/$wt)"
    printf 'release: %s ci run %s\n' "$gate" "$ran"
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
hash=$(nix_hash "$sha") ||
    die "pushed $sha but the nix hash failed; run: dev release --hash-only $sha"
request_block "$sha" "$hash"
