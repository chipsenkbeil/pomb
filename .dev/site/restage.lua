--[[
local
positional
flags: --bad
Make the next update real: dev site restage nightly [--bad]
]]
local site = require("pomb.site")

local BAD_FLAG = "--bad"
local SCRIPT = [==[
set -euo pipefail
exec 9>/run/lock/pomb-update-nightly.lock
if ! flock -n 9; then
  echo "restage: an update is running" >&2
  exit 1
fi
base=/opt/pomb/nightly
cur=$(readlink "$base/current") || {
  echo "restage: no current release" >&2
  exit 1
}
id=$(basename "$cur")
if [[ "$id" == before-* ]]; then
  echo "restage: current is already $id, run dev site deploy nightly force to replace it" >&2
  exit 1
fi
if [[ -n "${BAD:-}" && ! -f /var/lib/pomb/nightly/pomb.redb ]]; then
  echo "restage: --bad needs an existing pomb.redb" >&2
  exit 1
fi
if [[ -n "${BAD:-}" ]] && ! systemctl is-enabled --quiet pomb-nightly.service; then
  echo "restage: --bad needs pomb-nightly.service enabled" >&2
  exit 1
fi
prev="$base/releases/before-$id"
rm -rf "$prev"
cp -a "$cur" "$prev"
ln -sfn "$prev" "$base/current.new"
mv -T "$base/current.new" "$base/current"
rm -rf "$cur"
if [[ -n "${BAD:-}" ]]; then
  mkdir "$cur"
  printf '#!/bin/sh\nexit 1\n' >"$cur/pomb"
  chmod 755 "$cur/pomb"
  cp "$prev/commit" "$cur/commit"
fi
echo "nightly: current -> $(basename "$(readlink "$base/current")")"
]==]

local ctx = ...
local channel = ctx.args[1] or ""
local mode = ctx.args[2] or ""
if channel ~= site.CHANNEL.NIGHTLY or (mode ~= "" and mode ~= BAD_FLAG) then
    io.stderr:write("usage: dev site restage nightly [--bad]\n")
    return site.USAGE
end

return site.ssh_script(SCRIPT, { "BAD=" .. (mode ~= "" and "1" or ""), "bash", "-s" })
