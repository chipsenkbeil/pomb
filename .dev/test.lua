--[[
positional
Run the test suite. nextest skips doctests, so they get their own step.
Arguments go to nextest (`dev test -- -j 1 module`), doctests run only when there are none, each tests/js script runs with node against static/app.js, and every .dev/tests/*_test.lua runs last, arguments or not.
nextest runs with `INSTA_UPDATE=new`, so a failed run prints the path and text
of each pending `.snap.new` file.
]]
local devos = require("dev.os")
local fs = require("dev.fs")
local verb = require("dev.verb")

local ctx = ...
local argv = { "cargo", "nextest", "run", "--all-features" }
for _, arg in ipairs(ctx.args) do
    argv[#argv + 1] = arg
end

local function step(cmd)
    local code = devos.run(cmd, { cwd = ctx.root }) or 1
    if code ~= 0 then
        return code
    end
    return nil
end

local function print_pending_snapshots()
    local found = devos.capture({
        "find",
        ".",
        "-path",
        "./target",
        "-prune",
        "-o",
        "-name",
        "*.snap.new",
        "-type",
        "f",
        "-print",
    }, { cwd = ctx.root, stderr = "null" })
    local paths = {}
    for path in found:gmatch("[^\n]+") do
        paths[#paths + 1] = path
    end
    table.sort(paths)
    for _, path in ipairs(paths) do
        local file = assert(io.open(ctx.root .. "/" .. path, "rb"))
        local text = file:read("*a")
        file:close()
        verb.say("pending snapshot " .. path .. "\n" .. text)
    end
end

local code = devos.run(argv, { cwd = ctx.root, env = { INSTA_UPDATE = "new" } }) or 1
if code ~= 0 then
    print_pending_snapshots()
    return code
end

if #ctx.args == 0 then
    code = step({ "cargo", "test", "--doc", "--all-features" })
    if code then
        return code
    end
end

local names, message = fs.list(ctx.root .. "/tests/js")
if not names then
    verb.die(message)
end
for _, name in ipairs(names) do
    if name:sub(1, 1) ~= "." and name:match("%.js$") then
        code = step({ "node", "tests/js/" .. name, "static/app.js" })
        if code then
            return code
        end
    end
end
return require("dev.testrun").run(ctx.root)
