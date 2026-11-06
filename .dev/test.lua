--[[
positional
Run the test suite. nextest skips doctests, so they get their own step.
Arguments go to nextest (`dev test -- -j 1 module`), doctests run only when there are none, and each tests/js script runs with node against static/app.js.
]]
local devos = require("dev.os")
local fs = require("dev.fs")
local verb = require("dev.verb")

local ctx = ...
local argv = { "cargo", "nextest", "run", "--all-features" }
for _, arg in ipairs(ctx.args) do
    argv[#argv + 1] = arg
end

local code = devos.run(argv, { cwd = ctx.root }) or 1
if code ~= 0 then
    return code
end

if #ctx.args == 0 then
    code = devos.run({ "cargo", "test", "--doc", "--all-features" }, { cwd = ctx.root }) or 1
    if code ~= 0 then
        return code
    end
end

local names, message = fs.list(ctx.root .. "/tests/js")
if not names then
    verb.die(message)
end
for _, name in ipairs(names) do
    if name:sub(1, 1) ~= "." and name:match("%.js$") then
        code = devos.run({ "node", "tests/js/" .. name, "static/app.js" }, { cwd = ctx.root }) or 1
        if code ~= 0 then
            return code
        end
    end
end
return 0
