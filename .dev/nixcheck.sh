#!/usr/bin/env bash
# Run the test suite the way the Nix build does: no /usr/bin/env, a private TMPDIR, no network but
# loopback, and only the tools the pinned derivation provides on PATH.
#
#   dev nixcheck
#
# Builds the test archive with cargo, then runs it with nextest inside a user, mount and network
# namespace where /usr/bin/env is gone and PATH holds only the tools listed in the sandbox helper.
# A tool a test needs that the homelab derivation does not provide (stdenv's tools plus its
# nativeCheckInputs) fails here before a release is pushed. Only PATH is restricted: a test that
# runs a tool by absolute path still finds it. Doctests are not run. Linux only.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

archive="${CARGO_TARGET_DIR:-$root/target}/nixcheck.tar.zst"
case "$(realpath -m "$archive")" in
    /var/tmp | /var/tmp/*)
        printf 'nixcheck: %s is under /var/tmp, which the sandbox covers with a private mount; point CARGO_TARGET_DIR elsewhere\n' "$archive" >&2
        exit 1
        ;;
esac

unshare --user --map-root-user --mount --net true || {
    printf 'nixcheck: unprivileged user, mount and network namespaces are not available here\n' >&2
    exit 1
}

cargo nextest archive --all-features --archive-file "$archive"
nextest="$(command -v cargo-nextest)"
exec unshare --user --map-root-user --mount --net \
    "$root/.dev/__nixcheck_sandbox.sh" "$archive" "$root" "$(id -u)" "$(id -g)" "$nextest" "$@"
