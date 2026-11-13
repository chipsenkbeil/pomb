--[[
positional
Check the task finder of a candidate server against its state file.
flags: --candidate
]]
local devos = require("dev.os")
local argv = require("pomb.argv")

local ctx = ...
return devos.run(argv.node(ctx, "finder")) or 1
