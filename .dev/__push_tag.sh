#!/usr/bin/env bash
# Hidden helper: tag a released commit and push the tag to the remote of record.
#
#   .dev/__push_tag.sh <tag> <sha>
#
# Refuses a tag that already exists here or on the remote, and a commit that is
# not on its main, so a tag only ever names something that shipped.
set -euo pipefail

tag="${1:?usage: __push_tag.sh <tag> <sha>}"
rev="${2:?usage: __push_tag.sh <tag> <sha>}"

# shellcheck source=__remote.sh
. "$(dirname "${BASH_SOURCE[0]}")/__remote.sh"
remote=$(remote_of_record)

git fetch -q "$remote"
sha=$(git rev-parse -q --verify "$rev^{commit}") || { echo "unknown commit $rev" >&2; exit 1; }
git merge-base --is-ancestor "$sha" "$remote/main" || { echo "$sha is not on $remote/main" >&2; exit 1; }
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null || git ls-remote --exit-code --tags "$remote" "refs/tags/$tag" >/dev/null; then
    echo "tag $tag already exists" >&2
    exit 1
fi

git tag -a "$tag" "$sha" -m "Pomb ${tag#v}"
echo "tag     $tag at $sha"
jj-git-guard push -- "$remote" "refs/tags/$tag"
