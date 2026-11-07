--[[
hidden
local
positional
Remove the files a verifier planted in the nightly backups directory: dev site decoys
]]
local site = require("pomb.site")

local SCRIPT = [==[
set -euo pipefail
dir=/var/lib/pomb/nightly/backups
for name in notes.txt pomb-20200101T000000Z-before-x.redb.partial pomb-20200101T000000Z.redb; do
  if [[ -f "$dir/$name" && ! -s "$dir/$name" ]]; then
    rm -f "$dir/$name"
    echo "removed $name"
  fi
done
]==]

return site.ssh_script(SCRIPT, { "bash", "-s" })
