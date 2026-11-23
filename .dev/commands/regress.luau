local dev_command = require("@dev/command")
local devos = require("@dev/os")
local forge = require("@dev/forge")
local fs = require("@dev/fs")
local vcs = require("@lib/pomb/vcs")

local BASELINE = ".ci-baseline"
local INTERRUPTED = 130

local function resolve_sha(root, rev)
    local sha = vcs.resolve(root, rev)
    if not sha then
        dev_command.die("Cannot resolve " .. rev .. " to a commit.")
    end
    return sha
end

local function merge_base(root, remote_main)
    local sha = vcs.merge_base(root, remote_main)
    if not sha then
        dev_command.die("HEAD and " .. remote_main .. " have no merge-base.")
    end
    return sha
end

local function baseline_rev(root, requested)
    if requested ~= nil and requested ~= "" then
        return resolve_sha(root, requested)
    end
    local found, why = forge.resolve(root)
    if not found then
        dev_command.die(dev_command.vcs_sentence(why, root))
    end
    local code = devos.run({ "git", "fetch", "-q", found.remote, "main" }, { cwd = root }) or 1
    if code ~= 0 then
        return nil, code
    end
    return merge_base(root, found.remote .. "/main")
end

local function run_job(root, log, tree)
    return devos.run({
        "bash",
        "-o",
        "pipefail",
        "-c",
        'ci -C "$2" run --local --job regress -f | tee "$1"',
        "bash",
        log,
        tree,
    }, { cwd = root, env = { DEV_AGENT = "pomb/regress" } }) or 1
end

local function report(root, log)
    local text = fs.read(log) or ""
    local run = text:match("^[^\n]*"):match("^run (.*)$")
    if run == nil or run == "" then
        return
    end
    dev_command.note("run: " .. run)
    local listing = devos.capture({ "ci", "ps", run }, { cwd = root, stderr = "null" })
    local path
    for line in listing:gmatch("[^\n]+") do
        local field = line:match("^%s*(%S+)")
        if field and field:find("regress%-report%.tgz$") then
            path = field
            break
        end
    end
    if path then
        dev_command.note("report: ci cp " .. run .. ":" .. path .. " DST")
    else
        dev_command.note(
            "The regress report is not in the run's artifacts. Run ci ps "
                .. run
                .. " to list them."
        )
    end
end

return dev_command({
    summary = "Diff this tree's screenshots against a baseline revision on a ci browser runner.",
    runs_local = true,
    remote = false,
    positional = true,
    args = {
        { name = "rev", help = "the baseline revision, the merge-base with main by default" },
    },
    long = [[
REV defaults to the merge-base of HEAD and main on the remote of record, so a candidate behind main reports only its own diffs. The baseline sha prints to stderr before the run. A temporary directory holds a copy of the working copy's tracked and unignored files plus the revision exported to .ci-baseline/, the ci regress job runs on that directory, and the directory is removed on every exit, so the working tree is never written. The report, diff images included, is kept as a ci artifact, and the run id and the `ci cp` command that fetches it print at the end.
]],
    run = function(_, args, ctx)
        -- Resolve the baseline revision
        local root = ctx.root
        local rev, fetch_code = baseline_rev(root, args.rev)
        if not rev then
            return fetch_code
        end
        dev_command.note("baseline: " .. rev)

        -- Refuse when a baseline directory is already there
        if fs.exists(root .. "/" .. BASELINE) then
            dev_command.die("Refusing to start because " .. BASELINE .. "/ already exists.")
        end

        -- Make a scratch directory
        local dir, why = fs.mktemp({ prefix = "dev-regress", dir = true })
        if not dir then
            dev_command.die(why)
        end

        -- Stage the tree, export the baseline and run the job
        local ok, code = pcall(function()
            local tree = dir .. "/tree"
            if not fs.mkdir(tree) then
                return 1
            end
            local staged = vcs.stage(root, tree)
            if staged ~= 0 then
                return staged
            end
            local exported = vcs.export(root, rev, tree, BASELINE .. "/")
            if exported ~= 0 then
                return exported
            end
            local log = dir .. "/log"
            local status = run_job(root, log, tree)
            if status ~= INTERRUPTED then
                report(root, log)
            end
            return status
        end)

        -- Remove the scratch directory and pass on the result
        fs.remove(dir, { recursive = true })
        if not ok then
            error(code, 0)
        end
        return code
    end,
})
