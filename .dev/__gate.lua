--[[
hidden
positional
Run one dev gate on ci and fail unless ci tested the worktree.
Usage: dev __gate <worktree> <verb> <log>. Exactly one new ci run must appear under the worktree's name, and it must have passed from a clean local source.
]]
local devos = require("dev.os")
local fs = require("dev.fs")
local json = require("dev.json")

local ctx = ...

local function fail(...)
    io.stderr:write(...)
    io.stderr:write("\n")
    return 1
end

local function runs(wt)
    local out = devos.capture({ "ci", "-C", wt, "--json", "ps", "-a", "-n", "200" })
    local doc = json.decode(out) or {}
    local prefix = fs.basename(wt) .. "-"
    local rows = {}
    for _, row in ipairs(doc.runs or {}) do
        if type(row.repo) == "string" and row.repo:sub(1, #prefix) == prefix then
            rows[#rows + 1] = row
        end
    end
    return rows
end

local wt, verb, log = ctx.args[1], ctx.args[2], ctx.args[3]
if not (wt and verb and log) or wt == "" or verb == "" or log == "" then
    return fail("usage: __gate.sh <worktree> <verb> <log>")
end

local known = {}
for _, row in ipairs(runs(wt)) do
    known[row.run] = true
end
fs.remove(log)
local status = devos.run({ "dev", verb, "--remote" }, { cwd = wt, stdout = log, stderr = "stdout" })
if status ~= 0 then
    return fail("The command dev ", verb, " --remote failed in ", wt, ".")
end

local fresh = {}
for _, row in ipairs(runs(wt)) do
    if not known[row.run] then
        fresh[#fresh + 1] = row
    end
end
if #fresh ~= 1 then
    return fail(
        "The command dev ",
        verb,
        " --remote left ",
        #fresh,
        " new ci runs under ",
        wt,
        ", where 1 was expected, so it did not test this worktree."
    )
end

local row = fresh[1]
if row.status ~= "passed" or row.source ~= "local" then
    return fail(
        "The ci run ",
        row.run,
        " of ",
        row.repo,
        " is ",
        row.status,
        " from ",
        row.source,
        ", and only a passed run from local counts."
    )
end
io.stdout:write(row.run, " ", row.repo, " ", row.source, "\n")
return 0
