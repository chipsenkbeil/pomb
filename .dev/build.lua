--[[
positional
Build the release binary.
]]
local devos = require("dev.os")

local ctx = ...

local argv = { "cargo", "build", "--release", "--locked" }
for _, arg in ipairs(ctx.args) do
    argv[#argv + 1] = arg
end
return devos.run(argv, { cwd = ctx.root }) or 1
