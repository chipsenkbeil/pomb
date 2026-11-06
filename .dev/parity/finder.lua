--[[
positional
Check the task finder of a candidate server against its state file.
flags: --candidate
]]
local devos = require("dev.os")

local ctx = ...
local argv = { "node", ctx.root .. "/.dev/parity/finder.mjs" }
for _, arg in ipairs(ctx.args) do
    argv[#argv + 1] = arg
end
return devos.run(argv) or 1
