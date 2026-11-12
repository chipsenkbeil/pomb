--[[
local
positional
Show service states, current releases, last update results and disk use
]]
local site = require("pomb.site")

local ctx = ...
return site.ssh_script(site.read_script(ctx, "status"), { "bash", "-s" })
