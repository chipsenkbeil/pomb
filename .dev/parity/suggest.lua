--[[
positional
Print the Pomb element that overlaps each golden element of a manifest entry.
flags: --state --entry --manifest
]]
local devos = require("dev.os")
local argv = require("pomb.argv")

local ctx = ...
return devos.run(argv.node(ctx, "suggest")) or 1
