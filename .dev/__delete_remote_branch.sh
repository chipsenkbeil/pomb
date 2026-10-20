#!/usr/bin/env bash
# Hidden helper: delete branches on origin that the owner named as done.
#
#   .dev/__delete_remote_branch.sh <branch>...
#
# Prints each branch tip before deleting it, so a mistaken delete can be
# restored by pushing that sha back to the branch.
set -euo pipefail

[ "$#" -gt 0 ] || { echo 'usage: __delete_remote_branch.sh <branch>...' >&2; exit 2; }

git fetch -q origin
for branch in "$@"; do
    case "$branch" in
    main | HEAD | "") echo "refuse  $branch" >&2; exit 1 ;;
    esac
    tip=$(git rev-parse -q --verify "refs/remotes/origin/$branch") ||
        { echo "missing origin/$branch" >&2; exit 1; }
    echo "delete  origin/$branch at $tip"
    jj-git-guard push -- origin ":refs/heads/$branch"
done
