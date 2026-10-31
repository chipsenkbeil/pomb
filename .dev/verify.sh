#!/usr/bin/env bash
# Check a release on green-box, and clean up its worktrees.
# flags: --backup --cleanup --delete-branch
#
#   dev verify <release-sha>
#   dev verify <release-sha> --backup pomb-<ts>-before-<sha7>.redb
#   dev verify ... --cleanup .dev/worktrees/release-48 .dev/worktrees/verify-48
#   dev verify ... --cleanup <worktree>... --delete-branch fix/x
#
# Read-only checks against the live service over ssh, each printed as PASS or FAIL:
#   build_sha equals the release sha, prod app.js equals the release tree's, a
#   cross-site POST gets 403, the journal shows the schema and listening lines,
#   public /login returns 200, OpenAPI servers[0].url is the public URL, and a
#   bad Bearer token gets code token_invalid. --backup also checks that the
#   named database copy exists on the host. Any FAIL exits non-zero and
#   nothing else happens.
#
# On success the card moves to Done on the board: dev verify does not touch it.
#
# --cleanup removes the named .dev/worktrees/* and deletes their branches when
# fully merged into main on the remote of record; --delete-branch forces named branches too.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
main_repo="$(cd "$root" && cd "$(git rev-parse --git-common-dir)/.." && pwd)"

host="green-box"
public_url="https://pomb.chip.network"
backup_dir="/var/lib/pomb-backups"

sha="" backup=""
cleanup=()
force_branches=()

die() {
    printf 'verify: ABORT: %s\n' "$*" >&2
    exit 1
}

usage() {
    echo 'usage: dev verify <release-sha> [--backup <file>] [--cleanup <worktree>...] [--delete-branch <branch>]...' >&2
    exit 2
}

mode=""
while [ $# -gt 0 ]; do
    case "$1" in
    --backup) backup="${2:-}"; shift; mode="" ;;
    --cleanup) mode=cleanup ;;
    --delete-branch) force_branches+=("${2:-}"); shift; mode="" ;;
    -*) usage ;;
    *)
        if [ "$mode" = cleanup ]; then
            cleanup+=("$1")
        else
            [ -z "$sha" ] || usage
            sha="$1"
        fi
        ;;
    esac
    shift
done
[ -n "$sha" ] || usage

cd "$main_repo"

# shellcheck source=__remote.sh
. "$root/.dev/__remote.sh"
remote=$(remote_of_record) || exit 1
git fetch -q "$remote"
sha=$(git rev-parse --verify "$sha^{commit}") || die "no such commit: $sha"
failures=0

# check <name> <ok> <detail>: print one result line and count failures.
check() {
    if [ "$2" = 1 ]; then
        printf 'PASS  %-24s %s\n' "$1" "$3"
    else
        printf 'FAIL  %-24s %s\n' "$1" "$3"
        failures=$((failures + 1))
    fi
}

# remote <command>: run a command on the host.
# shellcheck disable=SC2029  # the command string is built client side on purpose
remote() {
    ssh "$host" "$1"
}

# --- post-switch checks ---
live_sha=$(remote 'curl -s localhost:3000/api/v1/version' | jq -r '.build_sha // empty' || true)
check build_sha "$([ "$live_sha" = "$sha" ] && echo 1 || echo 0)" "live ${live_sha:-none} release $sha"

prod_js=$(remote 'curl -s localhost:3000/static/app.js | shasum -a 256' | cut -d' ' -f1 || true)
tree_js=$(git show "$sha:static/app.js" | shasum -a 256 | cut -d' ' -f1)
check app_js "$([ -n "$prod_js" ] && [ "$prod_js" = "$tree_js" ] && echo 1 || echo 0)" "prod ${prod_js:0:12} release ${tree_js:0:12}"

xsite=$(remote 'curl -s -o /dev/null -w "%{http_code}" -X POST -H "Origin: https://evil.example" -H "Sec-Fetch-Site: cross-site" localhost:3000/api/boards' || true)
check cross_site_post "$([ "$xsite" = 403 ] && echo 1 || echo 0)" "status $xsite"

# shellcheck disable=SC2016  # the host expands the quoted command
journal=$(remote 'journalctl -u pomb --since "$(systemctl show pomb -p ActiveEnterTimestamp --value)" --no-pager | grep -iE "schema|listening" | tail -2' || true)
printf '%s\n' "$journal" | sed 's/^/      /'
journal_ok=0
if printf '%s\n' "$journal" | rg -qi 'schema' && printf '%s\n' "$journal" | rg -qi 'listening'; then
    journal_ok=1
fi
check journal "$journal_ok" "schema and listening lines since last start"

login_headers=$(curl -s -o /dev/null -D - "$public_url/login" | tr -d '\r' || true)
public=$(printf '%s\n' "$login_headers" | awk 'NR == 1 { print $2 }')
location=$(printf '%s\n' "$login_headers" | awk 'tolower($1) == "location:" { print $2 }')
case "$public:$location" in
    200:*) login_ok=1 login_case="200, the passkey sign-in page" ;;
    303:/) login_ok=1 login_case="303 to /, no sign-in (--auth none)" ;;
    303:/setup) login_ok=1 login_case="303 to /setup, passkey sign-in before the first passkey" ;;
    *) login_ok=0 login_case="status ${public:-none} location ${location:-none}" ;;
esac
check public_login "$login_ok" "$login_case"

servers=$(remote 'curl -s localhost:3000/api/v1/openapi.json' | jq -r '.servers[0].url // empty' || true)
check openapi_servers "$([ "$servers" = "$public_url" ] && echo 1 || echo 0)" "${servers:-none}"

code=$(remote 'curl -s -H "Authorization: Bearer pomb_nope" localhost:3000/api/boards' | jq -r '.code // empty' || true)
check bad_token_code "$([ "$code" = token_invalid ] && echo 1 || echo 0)" "code ${code:-none}"

if [ -n "$backup" ]; then
    present=0
    remote "sudo test -s '$backup_dir/$backup'" 2>/dev/null && present=1
    check backup_present "$present" "$backup_dir/$backup"
fi

if [ "$failures" -gt 0 ]; then
    printf '\nverify: %d check(s) FAILED; worktrees untouched\n' "$failures" >&2
    exit 1
fi
printf '\nverify: all checks passed\n'

# --- cleanup ---
for w in "${cleanup[@]+"${cleanup[@]}"}"; do
    w="${w%/}"
    case "$w" in
    .dev/worktrees/* | "$main_repo"/.dev/worktrees/*) ;;
    *) echo "skip    $w (not under .dev/worktrees)"; continue ;;
    esac
    branch=$(git -C "$w" symbolic-ref --short -q HEAD || true)
    if git worktree remove --force "$w"; then echo "removed $w"; else echo "skip    $w (not removable)"; continue; fi
    [ -n "$branch" ] || continue
    if git merge-base --is-ancestor "$branch" "$remote/main"; then
        git branch -q -d "$branch" && echo "deleted $branch (merged)"
    else
        case " ${force_branches[*]-} " in
        *" $branch "*) git branch -q -D "$branch" && echo "deleted $branch (forced)" ;;
        *) echo "kept    $branch (not merged; pass --delete-branch $branch)" ;;
        esac
    fi
done
