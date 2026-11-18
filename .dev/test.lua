--[[
Run the test suite. nextest skips doctests, so they get their own step.
Arguments go to nextest (`dev test -- -j 1 module`), and doctests run only when there are none.
nextest runs with `INSTA_UPDATE=new`, so a failed run prints the path and text
of each pending `.snap.new` file.
]]
local dev_os = require("dev.os")
local verb = require("dev.verb")

local ctx = ...
local argv = { "cargo", "nextest", "run", "--all-features" }
for _, arg in ipairs(ctx.args) do
    argv[#argv + 1] = arg
end

local function print_pending_snapshots()
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
    for _, path in ipairs(paths) do
        local file = assert(io.open(ctx.root .. "/" .. path, "rb"))
        local text = file:read("*a")
        file:close()
        verb.say(table.concat({ "pending snapshot ", path, "\n", text }))
    end
end

local code = dev_os.run(argv, { cwd = ctx.root, env = { INSTA_UPDATE = "new" } })
if code ~= 0 then
    print_pending_snapshots()
    code = code or 1
elseif #ctx.args == 0 then
    code = dev_os.run({ "cargo", "test", "--doc", "--all-features" }, { cwd = ctx.root }) or 1
end

-- --- project-specific tests below ---

local fs = require("dev.fs")

if code == 0 then
    local names, message = fs.list(ctx.root .. "/tests/js")
    if not names then
        verb.die(message)
    end
    for _, name in ipairs(names) do
        if code == 0 and name:sub(1, 1) ~= "." and name:match("%.js$") then
            code = dev_os.run({ "node", "tests/js/" .. name, "static/app.js" }, { cwd = ctx.root })
                or 1
        end
    end
end
if code == 0 then
    code = require("dev.testrun").run(ctx.root)
end

return code
