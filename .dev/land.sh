#!/usr/bin/env bash
# Apply a verified commit onto the remote's main and push it once lint and test pass on ci.
# flags: --card
#
#   dev land <verified-commit> --card ID
#
# Steps, any failure aborts before the push:
#   1. Fetch the remote of record. Refuse a commit that is not exactly one commit past its
#      merge-base with <remote>/main, or that changes the package version in Cargo.toml or
#      the pomb entry of Cargo.lock: only dev release bumps the version.
#   2. Apply the commit onto <remote>/main in .dev/worktrees/land-<ID>. On gerrit the commit
#      gets a Change-Id trailer when it has none.
#   3. Refuse attribution trailers through __push_main.sh --check, before any gate.
#   4. Run dev lint --remote, then dev test --remote, through __gate.sh, one at a time on ci,
#      logging to a temp dir. The first failing gate aborts.
#   5. Push to main through __push_main.sh, remove the worktree and print the landed sha.
#
# The remote of record is forge.remote, else gerrit when that remote exists, else origin.
# A failed land leaves its worktree behind; remove it before landing the card again.
# A push refused because main moved on exits 1 with the worktree left behind.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
main_repo="$(cd "$root" && cd "$(git rev-parse --git-common-dir)/.." && pwd)"
tools="$root/.dev"

# shellcheck source=__remote.sh
. "$tools/__remote.sh"

commit="" card=""

die() {
    printf 'land: ABORT: %s\n' "$*" >&2
    exit 1
}

usage() {
    echo 'usage: dev land <verified-commit> --card ID' >&2
    exit 2
}

versions() {
    git show "$1:Cargo.toml" | awk '
        /^\[/ { in_package = ($0 == "[package]") }
        in_package && /^version = "/ { gsub(/^version = "|"$/, ""); print; exit }'
    git show "$1:Cargo.lock" | awk '
        $0 == "name = \"pomb\"" { found = 1; next }
        found && /^version = "/ { gsub(/^version = "|"$/, ""); print; exit }'
}

while [ $# -gt 0 ]; do
    case "$1" in
    --card) [ $# -ge 2 ] || usage; card="$2"; shift ;;
    --card=*) card="${1#--card=}" ;;
    -*) usage ;;
    *)
        [ -z "$commit" ] || usage
        commit="$1"
        ;;
    esac
    shift
done
[ -n "$commit" ] && [ -n "$card" ] || usage
case "$card" in '' | *[!A-Za-z0-9]*) die "--card must be letters and digits, got '$card'" ;; esac

cd "$main_repo"
remote=$(remote_of_record) || exit 1
git fetch -q "$remote"
cand=$(git rev-parse --verify "$commit^{commit}") || die "no such commit: $commit"
base=$(git rev-parse "$remote/main")

fork=$(git merge-base "$base" "$cand") || die "$commit shares no history with $remote/main"
count=$(git rev-list --count "$fork..$cand")
[ "$count" = 1 ] || die "$commit is $count commits past $remote/main, expected exactly one"
[ "$(versions "$cand^")" = "$(versions "$cand")" ] ||
    die "$commit changes the package version in Cargo.toml or Cargo.lock; only dev release bumps it"

wt=".dev/worktrees/land-$card"
[ ! -e "$wt" ] || die "$wt already exists; remove it first"
git worktree add -q --detach "$wt" "$base"
cd "$wt"
if [ "$fork" = "$base" ]; then
    git reset -q --hard "$cand"
else
    git cherry-pick "$cand" >/dev/null || {
        git cherry-pick --abort || true
        die "$commit does not apply onto $remote/main (worktree left at $main_repo/$wt)"
    }
fi
if [ "$remote" = gerrit ]; then
    message=$(git log -1 --format=%B HEAD | git interpret-trailers --parse)
    case "$message" in *Change-Id:*) ;; *) add_change_id ;; esac
fi
sha=$(git rev-parse HEAD)

bash "$tools/__push_main.sh" "$PWD" --check || die "attribution check failed (worktree left at $main_repo/$wt)"

logs=$(mktemp -d "${TMPDIR:-/tmp}/land-$card.XXXXXX")
printf 'land: logs in %s\n' "$logs"
for gate in lint test; do
    printf 'land: dev %s --remote ...\n' "$gate"
    ran=$(bash "$tools/__gate.sh" "$PWD" "$gate" "$logs/$gate.log" 2>"$logs/$gate.err") ||
        die "dev $gate --remote did not pass on this worktree: $(cat "$logs/$gate.err"), see $logs/$gate.log (worktree left at $main_repo/$wt)"
    printf 'land: %s ci run %s\n' "$gate" "$ran"
done

status=0
bash "$tools/__push_main.sh" "$PWD" || status=$?
[ "$status" -eq 0 ] || die "push failed with status $status; 3 means $remote/main moved on (worktree left at $main_repo/$wt)"
cd "$main_repo"
git worktree remove --force "$wt"
printf 'land: landed %s\n' "$sha"
