--[[
positional
Build the release binary.
]]
local devos = require("dev.os")
local argv = require("pomb.argv")

local ctx = ...

local cmd = argv.extend({ "cargo", "build", "--release", "--locked" }, ctx.args)
return devos.run(cmd, { cwd = ctx.root }) or 1
