--[[
positional
Replay the parity manifest on a running Pomb and compare it with the goldens.
flags: --state --manifest --out --only --debug-dir
]]
local devos = require("dev.os")

local ctx = ...
local argv = { "node", ctx.root .. "/.dev/parity/run.mjs" }
for _, arg in ipairs(ctx.args) do
    argv[#argv + 1] = arg
end
return devos.run(argv) or 1
