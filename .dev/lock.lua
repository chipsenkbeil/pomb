--[[
local
positional
flags: --precise
Resolve Cargo.lock for one dependency on ci, with no cargo run on this machine.
Usage: dev lock PKG [--precise VER]
A temporary directory holds a copy of the working copy's tracked and unignored files, the ci lock job runs `cargo update -p PKG` on it, adding `--precise VER` when given, and builds and tests nothing. The run id prints to stderr, the job's Cargo.lock is fetched from the run's artifacts with `ci cp` and written over this tree's Cargo.lock, and nothing else here changes. The directory is removed on every exit.
]]
local devos = require("dev.os")
local fs = require("dev.fs")
local verb = require("dev.verb")

local ctx = ...
local root = ctx.root
local stderr = io.stderr
local USAGE = "Usage: dev lock PKG [--precise VER]"

local function parse(args)
    local found = {}
    local i = 1
    while i <= #args do
        local arg = args[i]
        local inline = arg:match("^%-%-precise=(.*)$")
        if inline or arg == "--precise" then
            if not inline then
                i = i + 1
                inline = args[i]
            end
            if inline == nil or inline == "" then
                verb.die("The --precise option needs a version. " .. USAGE)
            end
            found.precise = inline
        elseif found.package == nil and arg:sub(1, 1) ~= "-" then
            found.package = arg
        else
            verb.die("Cannot use the argument '" .. arg .. "'. " .. USAGE)
        end
        i = i + 1
    end
    if found.package == nil or found.package == "" then
        verb.die("Name the package to resolve. " .. USAGE)
    end
    return found
end

local function stage(tree)
    return devos.run({
        "bash",
        "-o",
        "pipefail",
        "-c",
        'git ls-files -z --cached --others --exclude-standard | while IFS= read -r -d "" path; do '
            .. 'if [ -e "$path" ] || [ -L "$path" ]; then printf "%s\\0" "$path"; fi; done '
            .. '| tar -cf - --null -T - | tar -xf - -C "$1"',
        "bash",
        tree,
    }, { cwd = root }) or 1
end

local function run_job(log, tree, wanted)
    local argv = {
        "bash",
        "-o",
        "pipefail",
        "-c",
        'ci -C "$2" run --local --job lock -f -- "${@:3}" | tee "$1"',
        "bash",
        log,
        tree,
        wanted.package,
    }
    if wanted.precise then
        argv[#argv + 1] = wanted.precise
    end
    return devos.run(argv, { cwd = root, env = { DEV_AGENT = "pomb/lock" } }) or 1
end

local function artifact(run)
    local listing = devos.capture({ "ci", "ps", run }, { cwd = root, stderr = "null" })
    for line in listing:gmatch("[^\n]+") do
        local field = line:match("^%s*(%S+)")
        if field and (field == "Cargo.lock" or field:find("/Cargo%.lock$")) then
            return field
        end
    end
end

local function fetch(run, dir)
    local path = artifact(run)
    if not path then
        stderr:write(
            "The lock job left no Cargo.lock in the run's artifacts. Run ci ps ",
            run,
            " to list them.\n"
        )
        return 1
    end
    local fetched = dir .. "/fetched"
    if not fs.mkdir(fetched) then
        return 1
    end
    local code = devos.run({ "ci", "cp", run .. ":" .. path, fetched }, { cwd = root }) or 1
    if code ~= 0 then
        return code
    end
    local lock = fs.read(fetched .. "/Cargo.lock")
    if lock == nil or lock == "" then
        stderr:write("The fetched Cargo.lock is missing or empty.\n")
        return 1
    end
    if not fs.write(root .. "/Cargo.lock", lock) then
        stderr:write("Cannot write Cargo.lock.\n")
        return 1
    end
    return 0
end

local wanted = parse(ctx.args)

local dir, why = fs.mktemp({ prefix = "dev-lock", dir = true })
if not dir then
    verb.die(why)
end

local ok, code = pcall(function()
    local tree = dir .. "/tree"
    if not fs.mkdir(tree) then
        return 1
    end
    local staged = stage(tree)
    if staged ~= 0 then
        return staged
    end
    local log = dir .. "/log"
    local status = run_job(log, tree, wanted)
    local text = fs.read(log) or ""
    local run = text:match("^[^\n]*"):match("^run (.*)$")
    if run ~= nil and run ~= "" then
        stderr:write("run: ", run, "\n")
    end
    if status ~= 0 then
        return status
    end
    if run == nil or run == "" then
        stderr:write("The lock job printed no run id.\n")
        return 1
    end
    return fetch(run, dir)
end)

fs.remove(dir, { recursive = true })
if not ok then
    error(code, 0)
end
return code
