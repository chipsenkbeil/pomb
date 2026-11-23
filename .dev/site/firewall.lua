--[[
local
positional
Close port 22 to everything but the tunnel: dev site firewall
]]
local site = require("pomb.site")
local dev_command = require("dev.command")

local ctx = ...
local override = os.getenv("POMB_HOST")
if override ~= nil and override ~= "" then
    dev_command.die(
        "site: firewall runs only through " .. site.TUNNEL_HOST .. ", so unset POMB_HOST"
    )
end
return site.ssh_script(site.read_script(ctx, "firewall"), { "bash", "-s" })
