local dev_command = require("@dev/command")
local dev_os = require("@dev/os")
local fs = require("@dev/fs")
local vcs = require("@lib/pomb/vcs")

local USAGE = "Usage: dev lock PKG [--precise VER]"

local function run_job(root, log, tree, package, precise)
    local argv = {
        "bash",
        "-o",
        "pipefail",
        "-c",
        'ci -C "$2" run --local --job lock -f -- "${@:3}" | tee "$1"',
        "bash",
        log,
        tree,
        package,
    }
    if precise then
        argv[#argv + 1] = precise
    end
    return dev_os.run(argv, { cwd = root, env = { DEV_AGENT = "pomb/lock" } }) or 1
end

local function artifact(root, run)
    local listing = dev_os.capture({ "ci", "ps", run }, { cwd = root, stderr = "null" })
    for line in listing:gmatch("[^\n]+") do
        local field = line:match("^%s*(%S+)")
        if field and (field == "Cargo.lock" or field:find("/Cargo%.lock$")) then
            return field
        end
    end
end

local function fetch(root, run, dir)
    local path = artifact(root, run)
    if not path then
        dev_command.note(
            "The lock job left no Cargo.lock in the run's artifacts. Run ci ps "
                .. run
                .. " to list them."
        )
        return 1
    end
    local fetched = dir .. "/fetched"
    if not fs.mkdir(fetched) then
        return 1
    end
    local code = dev_os.run({ "ci", "cp", run .. ":" .. path, fetched }, { cwd = root }) or 1
    if code ~= 0 then
        return code
    end
    local lock = fs.read(fetched .. "/Cargo.lock")
    if lock == nil or lock == "" then
        dev_command.note("The fetched Cargo.lock is missing or empty.")
        return 1
    end
    if not fs.write(root .. "/Cargo.lock", lock) then
        dev_command.note("Cannot write Cargo.lock.")
        return 1
    end
    return 0
end

return dev_command({
    summary = "Resolve Cargo.lock for one dependency on ci, with no cargo run on this machine.",
    runs_local = true,
    remote = false,
    positional = true,
    requires = { "workspace" },
    flags = {
        { name = "precise", kind = "string", help = "pin the dependency to this version" },
    },
    args = {
        { name = "pkg", required = true, help = "the dependency to resolve" },
    },
    long = [[
Usage: dev lock PKG [--precise VER]
A temporary directory holds a copy of the working copy's tracked and unignored files, the ci lock job runs `cargo update -p PKG` on it, adding `--precise VER` when given, and builds and tests nothing. The run id prints to stderr, the job's Cargo.lock is fetched from the run's artifacts with `ci cp` and written over this tree's Cargo.lock, and nothing else here changes. The directory is removed on every exit.
]],
    run = function(opts, args, ctx)
        -- Check the arguments
        local root = ctx.root
        local precise = opts.precise
        if precise == "" then
            dev_command.die("The --precise option needs a version. " .. USAGE)
        end
        if args.pkg == "" then
            dev_command.die("Name the package to resolve. " .. USAGE)
        end

        -- Make a scratch directory
        local dir, why = fs.mktemp({ prefix = "dev-lock", dir = true })
        if not dir then
            dev_command.die(why)
        end

        -- Stage the tree and run the lock job
        local ok, code = pcall(function()
            local tree = dir .. "/tree"
            if not fs.mkdir(tree) then
                return 1
            end
            local staged = vcs.stage(root, tree)
            if staged ~= 0 then
                return staged
            end
            local log = dir .. "/log"
            local status = run_job(root, log, tree, args.pkg, precise)
            local text = fs.read(log) or ""
            local run = text:match("^[^\n]*"):match("^run (.*)$")
            if run ~= nil and run ~= "" then
                dev_command.note("run: " .. run)
            end
            if status ~= 0 then
                return status
            end
            if run == nil or run == "" then
                dev_command.note("The lock job printed no run id.")
                return 1
            end
            return fetch(root, run, dir)
        end)

        -- Remove the scratch directory and pass on the result
        fs.remove(dir, { recursive = true })
        if not ok then
            error(code, 0)
        end
        return code
    end,
})
