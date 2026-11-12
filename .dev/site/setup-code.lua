--[[
local
positional
Print the setup code each channel logged at its first start: dev site setup-code
]]
local site = require("pomb.site")

local ctx = ...
return site.ssh_script(site.read_script(ctx, "setup-code"), { "bash", "-s" })
