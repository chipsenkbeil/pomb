--[[
positional
Diff the screenshots of two running Pomb builds entry by entry.
flags: --candidate --baseline --out --manifest --only --max-diff --fuzz
]]
local devos = require("dev.os")

local ctx = ...
local argv = { "node", ctx.root .. "/.dev/parity/regress.mjs" }
for _, arg in ipairs(ctx.args) do
    argv[#argv + 1] = arg
end
return devos.run(argv) or 1
