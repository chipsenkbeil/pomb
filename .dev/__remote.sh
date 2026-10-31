#!/usr/bin/env bash
# Hidden helper, sourced: the remote of record,
# the Change-Id trailer for Gerrit and the wait for the GitHub mirror.

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

# wait_for_mirror <sha> <seconds> <interval>: poll origin (GitHub) until main contains sha.
wait_for_mirror() {
    local sha=$1 secs=$2 step=$3 waited=0
    while :; do
        if git fetch -q origin main && git merge-base --is-ancestor "$sha" FETCH_HEAD 2>/dev/null; then
            return 0
        fi
        [ "$waited" -lt "$secs" ] || break
        printf 'mirror: origin main does not hold %s yet, waited %ss of %ss\n' "${sha:0:7}" "$waited" "$secs" >&2
        sleep "$step"
        waited=$((waited + step))
    done
    printf 'mirror: origin main did not reach %s within %ss\n' "$sha" "$secs" >&2
    return 1
}
