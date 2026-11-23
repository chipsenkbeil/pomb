--[[
Run the test suite. nextest skips doctests, so they get their own step.
positional
Arguments go to nextest (`dev test -- -j 1 module`), and doctests run only when there are none.
nextest runs with `INSTA_UPDATE=new`, so a failed run prints the path and text
of each pending `.snap.new` file.
]]
local dev_os = require("dev.os")
local dev_command = require("dev.command")

local ctx = ...
local argv = { "cargo", "nextest", "run", "--all-features" }
for _, arg in ipairs(ctx.args) do
    argv[#argv + 1] = arg
end

local function step(cmd)
    local code = dev_os.run(cmd, { cwd = ctx.root }) or 1
    if code ~= 0 then
        return code
    end
    return nil
end

local function find_pending_snapshots()
    local found = dev_os.capture({
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
    return paths
end

local function clear_pending_snapshots()
    for _, path in ipairs(find_pending_snapshots()) do
        assert(os.remove(ctx.root .. "/" .. path))
    end
end

local function print_pending_snapshots()
    for _, path in ipairs(find_pending_snapshots()) do
        local file = assert(io.open(ctx.root .. "/" .. path, "rb"))
        local text = file:read("*a")
        file:close()
        dev_command.say(table.concat({ "pending snapshot ", path, "\n", text }))
    end
end

clear_pending_snapshots()
local code = dev_os.run(argv, { cwd = ctx.root, env = { INSTA_UPDATE = "new" } }) or 1
if code ~= 0 then
    print_pending_snapshots()
    return code
end

if #ctx.args == 0 then
    local failed = step({ "cargo", "test", "--doc", "--all-features" })
    if failed then
        return failed
    end
end

-- --- project-specific tests below ---

local fs = require("dev.fs")

local names, message = fs.list(ctx.root .. "/tests/js")
if not names then
    dev_command.die(message)
end
for _, name in ipairs(names) do
    if name:sub(1, 1) ~= "." and name:match("%.js$") then
        local failed = step({ "node", "tests/js/" .. name, "static/app.js" })
        if failed then
            return failed
        end
    end
end

code = require("dev.testrun").run(ctx.root)

return code
