#!/usr/bin/env bash
# Hidden helper, sourced: the remote of record and the Change-Id trailer for Gerrit.

# remote_of_record [repo]: print forge.remote, else gerrit when that remote exists, else origin.
remote_of_record() {
    local repo=${1:-.} remote
    remote=$(git -C "$repo" config --get forge.remote || true)
    if [ -z "$remote" ]; then
        if git -C "$repo" remote | grep -qx gerrit; then remote=gerrit; else remote=origin; fi
    fi
    git -C "$repo" remote | grep -qx -- "$remote" || {
        printf 'remote of record %s is not a remote of this repo\n' "$remote" >&2
        return 1
    }
    printf '%s\n' "$remote"
}

# add_change_id: amend HEAD with a Change-Id trailer, hashed from the commit as Gerrit's commit-msg hook does.
add_change_id() {
    local id
    id=$(git cat-file commit HEAD | git hash-object -t commit --stdin)
    { git log -1 --format=%B HEAD; printf '\nChange-Id: I%s\n' "$id"; } | git commit -q --amend -F -
}
