--[[
local
positional
Diff this tree's screenshots against a baseline revision on a ci browser runner.
REV defaults to main on the remote of record. The revision is exported to .ci-baseline/, the ci regress job runs on the working copy, and .ci-baseline/ is removed on every exit. The report, diff images included, is kept as a ci artifact, and the run id and the `ci cp` command that fetches it print at the end.
]]
local devos = require("dev.os")
local forge = require("dev.forge")
local fs = require("dev.fs")
local verb = require("dev.verb")

local ctx = ...
local root = ctx.root
local stderr = io.stderr
local BASELINE = ".ci-baseline"
local INTERRUPTED = 130

local function baseline_rev(requested)
    if requested ~= nil and requested ~= "" then
        return requested
    end
    local found, why = forge.resolve(root)
    if not found then
        verb.die(why)
    end
    local code = devos.run({ "git", "fetch", "-q", found.remote, "main" }, { cwd = root }) or 1
    if code ~= 0 then
        return nil, code
    end
    return found.remote .. "/main"
end

local function export(rev, dir)
    local archive = dir .. "/baseline.tar"
    local code = devos.run({
        "git",
        "archive",
        "--output=" .. archive,
        "--prefix=" .. BASELINE .. "/",
        rev,
    }, { cwd = root }) or 1
    if code ~= 0 then
        return code
    end
    return devos.run({ "tar", "-xf", archive }, { cwd = root }) or 1
end

local function run_job(log)
    return devos.run({
        "bash",
        "-o",
        "pipefail",
        "-c",
        'ci run --local --job regress -f | tee "$1"',
        "bash",
        log,
    }, { cwd = root, env = { DEV_AGENT = "pomb/regress" } }) or 1
end

local function report(log)
    local text = fs.read(log) or ""
    local run = text:match("^[^\n]*"):match("^run (.*)$")
    if run == nil or run == "" then
        return
    end
    stderr:write("run: ", run, "\n")
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
        stderr:write("report: ci cp ", run, ":", path, " DST\n")
    else
        stderr:write(
            "The regress report is not in the run's artifacts. Run ci ps ",
            run,
            " to list them.\n"
        )
    end
end

local rev, fetch_code = baseline_rev(ctx.args[1])
if not rev then
    return fetch_code
end

if fs.exists(root .. "/" .. BASELINE) then
    verb.die("Refusing to start because " .. BASELINE .. "/ already exists.")
end

local dir, why = fs.mktemp({ prefix = "dev-regress", dir = true })
if not dir then
    verb.die(why)
end

local ok, code = pcall(function()
    local exported = export(rev, dir)
    if exported ~= 0 then
        return exported
    end
    local log = dir .. "/log"
    local status = run_job(log)
    if status ~= INTERRUPTED then
        report(log)
    end
    return status
end)

fs.remove(root .. "/" .. BASELINE, { recursive = true })
fs.remove(dir, { recursive = true })
if not ok then
    error(code, 0)
end
return code
