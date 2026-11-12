--[[
hidden
local
positional
Remove the files a verifier planted in the nightly backups directory: dev site decoys
]]
local site = require("pomb.site")

local ctx = ...
return site.ssh_script(site.read_script(ctx, "decoys"), { "bash", "-s" })
