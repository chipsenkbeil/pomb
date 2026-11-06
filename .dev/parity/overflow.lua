--[[
positional
Check that the candidate pages do not scroll sideways at a phone width.
]]
local devos = require("dev.os")

local ctx = ...
local argv = { "node", ctx.root .. "/.dev/parity/overflow.mjs" }
for _, arg in ipairs(ctx.args) do
    argv[#argv + 1] = arg
end
return devos.run(argv) or 1
