--[[
positional
Diff the screenshots of two running Pomb builds entry by entry.
flags: --candidate --baseline --out --manifest --only --max-diff --fuzz
]]
local devos = require("dev.os")
local argv = require("pomb.argv")

local ctx = ...
return devos.run(argv.node(ctx, "regress")) or 1
