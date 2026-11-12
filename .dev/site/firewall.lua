--[[
local
positional
Close port 22 to everything but the tunnel: dev site firewall
]]
local site = require("pomb.site")
local verb = require("dev.verb")

local ctx = ...
local override = os.getenv("POMB_HOST")
if override ~= nil and override ~= "" then
    verb.die("site: firewall runs only through " .. site.TUNNEL_HOST .. ", so unset POMB_HOST")
end
return site.ssh_script(site.read_script(ctx, "firewall"), { "bash", "-s" })
