--!strict
local dev_command = require("@dev/command")
local dev_os = require("@dev/os")
local fs = require("@dev/fs")

type Ctx = {
    ---absolute path of the project root
    root: string,
}

local LONG = [[
nextest skips doctests, so they get their own step. Arguments go to nextest (`dev test -- -j 1
module`), and doctests run only when there are none. nextest runs with `INSTA_UPDATE=new`, so a
failed run prints the path and text of each pending `.snap.new` file.]]

local function find_pending_snapshots(root: string): { string }
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
    }, { cwd = root, stderr = "null" })
    local paths: { string } = {}
    for path in found:gmatch("[^\n]+") do
        paths[#paths + 1] = path
    end
    table.sort(paths)
    return paths
end

local function clear_pending_snapshots(root: string)
    for _, path in ipairs(find_pending_snapshots(root)) do
        assert(fs.remove(fs.join(root, path)))
    end
end

local function print_pending_snapshots(root: string)
    for _, path in ipairs(find_pending_snapshots(root)) do
        local text = fs.read(fs.join(root, path)) or ""
        dev_command.say(`pending snapshot {path}\n{text}`)
    end
end

return dev_command({
    summary = "Run the Rust tests with nextest and the doctests",
    long = LONG,
    group = "Check your work",
    positional = true,
    pass_unknown = true,
    args = { { name = "nextest", list = true, rest = true, help = "arguments for nextest" } },
    run = function(_: unknown, args: { nextest: { string }? }, ctx: Ctx): number
        local root = ctx.root
        local words = args.nextest or {}
        local argv = { "cargo", "nextest", "run", "--all-features" }
        for _, word in ipairs(words) do
            argv[#argv + 1] = word
        end

        clear_pending_snapshots(root)
        local code = dev_os.run(argv, { cwd = root, env = { INSTA_UPDATE = "new" } }) or 1
        if code ~= 0 then
            print_pending_snapshots(root)
            return code
        end

        if #words == 0 then
            local doc = dev_os.run({ "cargo", "test", "--doc", "--all-features" }, { cwd = root })
            if doc ~= 0 then
                return doc or 1
            end
        end

        -- --- project-specific tests below ---

        local names, message = fs.list(root .. "/tests/js")
        if not names then
            dev_command.die(message)
        end
        for _, name: string in ipairs(names :: { string }) do
            if name:sub(1, 1) ~= "." and name:match("%.js$") then
                local failed = dev_os.run(
                    { "node", "tests/js/" .. name, "static/app.js" },
                    { cwd = root }
                )
                if failed ~= 0 then
                    return failed or 1
                end
            end
        end

        code = require("@dev/testrun").run(root)

        return code
    end,
})
