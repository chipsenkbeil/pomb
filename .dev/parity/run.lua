--[[
positional
Replay the parity manifest on a running Pomb and compare it with the goldens.
flags: --state --manifest --out --only --debug-dir
]]
local devos = require("dev.os")
local argv = require("pomb.argv")

local ctx = ...
return devos.run(argv.node(ctx, "run")) or 1
