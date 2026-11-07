#!/usr/bin/env bash
# Hidden helper: delete branches on the remote of record that the owner named as done.
#
#   .dev/__delete_remote_branch.sh <branch>...
#
# Prints each branch tip before deleting it, so a mistaken delete can be
# restored by pushing that sha back to the branch.
set -euo pipefail

[ "$#" -gt 0 ] || { echo 'usage: __delete_remote_branch.sh <branch>...' >&2; exit 2; }

# shellcheck source=__remote.sh
. "$(dirname "${BASH_SOURCE[0]}")/__remote.sh"
remote=$(remote_of_record)

git fetch -q "$remote"
for branch in "$@"; do
    case "$branch" in
    main | HEAD | "") echo "refuse  $branch" >&2; exit 1 ;;
    esac
    tip=$(git rev-parse -q --verify "refs/remotes/$remote/$branch") ||
        { echo "missing $remote/$branch" >&2; exit 1; }
    echo "delete  $remote/$branch at $tip"
    jj-git-guard push -- "$remote" ":refs/heads/$branch"
done
