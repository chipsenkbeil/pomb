#!/usr/bin/env bash
# standalone: runs on test sandbox, where dev is absent
# Hidden helper: inside the namespaces of nixcheck.sh, give /var/tmp a private mount, copy the
# allowed tools into it, hide /usr/bin/env and run the archived tests as the caller's own user.
set -euo pipefail

archive="${1:?usage: __nixcheck_sandbox.sh <archive> <root> <uid> <gid> <nextest> [nextest args]}"
root="${2:?}" uid="${3:?}" gid="${4:?}" nextest="${5:?}"
shift 5

allowed=(
    bash sh cat cp mv rm mkdir rmdir ls ln chmod touch date sleep sort uniq tr head tail wc cut tee
    env basename dirname readlink realpath mktemp comm kill id true false test expr seq stat tac
    sed grep awk find xargs tar gzip diff cmp jq git rg
)

type -P ip >/dev/null || {
    printf 'The sandbox needs ip (iproute2) to bring up the loopback interface.\n' >&2
    exit 1
}
[ -e /usr/bin/env ] || {
    printf 'The file /usr/bin/env does not exist on this host.\n' >&2
    exit 1
}

mount -t tmpfs tmpfs /var/tmp
bin="$(mktemp -d /var/tmp/tools.XXXXXX)"
for tool in "${allowed[@]}"; do
    path="$(type -P "$tool")" || {
        printf 'The allowed tool %s is not installed.\n' "$tool" >&2
        exit 1
    }
    cp -L "$path" "$bin/$tool"
done

ip link set lo up
mount --bind /dev/null /usr/bin/env

exec unshare --user --map-user="$uid" --map-group="$gid" \
    "$bin/env" -i PATH="$bin" HOME=/homeless-shelter TZ=UTC TMPDIR=/var/tmp \
    "$nextest" nextest run --archive-file "$archive" --workspace-remap "$root" "$@"
