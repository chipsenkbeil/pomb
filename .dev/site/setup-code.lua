--[[
local
positional
Print the setup code each channel logged at its first start: dev site setup-code
]]
local site = require("pomb.site")

local SCRIPT = [==[
for ch in stable nightly; do
  printf '%-8s %s\n' "$ch" "$(journalctl -u "pomb-$ch" --no-pager -q | grep "setup code" | tail -n 1 | sed 's/.*setup code/setup code/')"
done
]==]

return site.ssh_script(SCRIPT, { "bash", "-s" })
