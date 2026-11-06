--[[
positional
Print the Pomb element that overlaps each golden element of a manifest entry.
flags: --state --entry --manifest
]]
local devos = require("dev.os")

local ctx = ...
local argv = { "node", ctx.root .. "/.dev/parity/suggest.mjs" }
for _, arg in ipairs(ctx.args) do
    argv[#argv + 1] = arg
end
return devos.run(argv) or 1
