--[[
positional
Check that the candidate pages do not scroll sideways at a phone width.
]]
local devos = require("dev.os")
local argv = require("pomb.argv")

local ctx = ...
return devos.run(argv.node(ctx, "overflow")) or 1
