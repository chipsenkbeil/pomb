#!/usr/bin/env bash
# Cut a release from the remote's main as it stands and print the homelab request.
# flags: --batch --summary --env --dry-run --hash-only
#
#   dev release --batch 48 --summary "limits and docs"
#   dev release --batch 48 --summary "..." --env KEY=VALUE
#   dev release --batch 48 --summary "..." --dry-run
#
# Each card lands on main through dev land after its own verification, so a release adds
# only the version bump. Steps, any failure aborts before the push:
#   1. Refuse unless <remote>/main has a commit since its last v* tag.
#   2. Bump the version with .dev/__bump_version.sh in .dev/worktrees/release-<N> on
#      <remote>/main and commit "Release batch-<N>: <summary>" with only Cargo.toml and
#      Cargo.lock. Then refuse unless the new version differs from the last v* tag's and
#      Cargo.lock's pomb entry matches it.
#   3. Run dev lint --remote, dev test --remote, dev nixcheck --remote and dev build --remote,
#      one at a time on ci, logging to a temp dir. A failing gate aborts before the push, and
#      so does a gate whose ci run is not a new passed run of the release worktree with
#      source local.
#   4. Refuse attribution trailers in the new commit and refuse when <remote>/main moved since
#      step 1, then push to main on the remote of record.
#   5. Tag batch-<N> and v<version> at the pushed commit with .dev/__push_tag.sh, then compute
#      the nix hash of the commit's tree and print the request block. A failed tag prints the
#      __push_tag.sh command to rerun for it.
#
# The remote of record is forge.remote, else gerrit when that remote exists, else origin.
# On gerrit the release commit carries a Change-Id trailer.
#
# --hash-only <sha> fetches main from the remote of record and prints the request block
# for an already pushed sha: the recovery path when the hash step fails after the push.
#
# --dry-run does everything except the push, the tags and the hash. The release worktree
# stays behind. dev verify --cleanup removes it once the release is live.
# Repeat --env to list pomb.service environment changes in the request.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
main_repo="$(cd "$root" && cd "$(git rev-parse --git-common-dir)/.." && pwd)"
tools="$root/.dev"

# shellcheck source=__remote.sh
. "$tools/__remote.sh"

batch="" summary="" dry_run=0 hash_only=""
envs=()

die() {
    printf 'release: ABORT: %s\n' "$*" >&2
    exit 1
}

usage() {
    echo 'usage: dev release --batch N --summary "<text>" [--env KEY=VALUE]... [--dry-run]' >&2
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
    *) usage ;;
    esac
    shift
done
if [ -n "$hash_only" ]; then
    [ -z "$summary" ] || usage
else
    [ -n "$batch" ] && [ -n "$summary" ] || usage
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
base=$(git rev-parse "$remote/main")

# 1. main must hold a commit since the last tag
last_tag=$(git describe --tags --match 'v*' --abbrev=0 "$base") || die "no v* tag in the history of $remote/main"
[ "$(git rev-list --count "$last_tag..$base")" -gt 0 ] ||
    die "$remote/main (${base:0:7}) has no commit since $last_tag; nothing to release"

# 2. bump the version in a fresh worktree and commit only that
wt=".dev/worktrees/release-$batch"
[ ! -e "$wt" ] || die "$wt already exists; remove it first"
git worktree add -q --detach "$wt" "$base"
cd "$wt"
new_version=$(bash "$tools/__bump_version.sh" "$PWD") || die "version bump failed (worktree left at $main_repo/$wt)"
git add Cargo.toml Cargo.lock
git commit -q -m "Release batch-$batch: $summary"
[ "$remote" != gerrit ] || add_change_id
sha=$(git rev-parse HEAD)

tag_toml=$(mktemp)
git show "$last_tag:Cargo.toml" >"$tag_toml" || { rm -f "$tag_toml"; die "cannot read Cargo.toml at $last_tag"; }
guard_ok=0
bash "$tools/__version_guard.sh" "$tag_toml" Cargo.toml Cargo.lock || guard_ok=1
rm -f "$tag_toml"
[ "$guard_ok" = 0 ] || die "version $new_version fails the check against $last_tag (worktree left at $main_repo/$wt)"

# 3. gates, one at a time on ci
logs=$(mktemp -d "${TMPDIR:-/tmp}/release-$batch.XXXXXX")
printf 'release: logs in %s\n' "$logs"
export CARGO_TARGET_DIR="$PWD/target"
for gate in lint test nixcheck build; do
    printf 'release: dev %s --remote ...\n' "$gate"
    ran=$(bash "$tools/__gate.sh" "$PWD" "$gate" "$logs/$gate.log" 2>"$logs/$gate.err") ||
        die "dev $gate --remote did not pass on this worktree: $(cat "$logs/$gate.err"), see $logs/$gate.log (worktree left at $main_repo/$wt)"
    printf 'release: %s ci run %s\n' "$gate" "$ran"
done
printf 'release: gates ok: %s\n' "$(rg -o 'Summary.*' "$logs/test.log" | tail -1)"

# 4. attribution check, then push
bash "$tools/__push_main.sh" "$PWD" --check || die "attribution check failed (worktree left at $main_repo/$wt)"
git fetch -q "$remote"
[ "$(git rev-parse "$remote/main")" = "$base" ] ||
    die "$remote/main moved past ${base:0:7} while the gates ran; rerun dev release (worktree left at $main_repo/$wt)"
if [ "$dry_run" -eq 1 ]; then
    printf 'release: dry run, not pushed. Release commit %s in %s/%s\n' "$sha" "$main_repo" "$wt"
    exit 0
fi
bash "$tools/__push_main.sh" "$PWD" || die "push failed (worktree left at $main_repo/$wt)"

# 5. tags, nix hash and the request
tag_reruns=""
for tag in "batch-$batch" "v$new_version"; do
    bash "$tools/__push_tag.sh" "$tag" "$sha" || tag_reruns+=" && bash $tools/__push_tag.sh $tag $sha"
done
[ -z "$tag_reruns" ] || die "pushed $sha but tagging failed; rerun: ${tag_reruns# && }, then dev release --hash-only $sha"
hash=$(nix_hash "$sha") ||
    die "pushed $sha but the nix hash failed; run: dev release --hash-only $sha"
request_block "$sha" "$hash"
