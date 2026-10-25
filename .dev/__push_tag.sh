#!/usr/bin/env bash
# Hidden helper: tag a released commit and push the tag to origin.
#
#   .dev/__push_tag.sh <tag> <sha>
#
# Refuses a tag that already exists here or on origin, and a commit that is
# not on origin/main, so a tag only ever names something that shipped.
set -euo pipefail

tag="${1:?usage: __push_tag.sh <tag> <sha>}"
rev="${2:?usage: __push_tag.sh <tag> <sha>}"

git fetch -q origin
sha=$(git rev-parse -q --verify "$rev^{commit}") || { echo "unknown commit $rev" >&2; exit 1; }
git merge-base --is-ancestor "$sha" origin/main || { echo "$sha is not on origin/main" >&2; exit 1; }
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null || git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null; then
    echo "tag $tag already exists" >&2
    exit 1
fi

git tag -a "$tag" "$sha" -m "Pomb ${tag#v}"
echo "tag     $tag at $sha"
jj-git-guard push -- origin "refs/tags/$tag"
