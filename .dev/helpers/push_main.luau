---Refuse attribution trailers, then push HEAD to main on the remote of record.
---Usage: dev -l push_main.luau <worktree> [--check]. --check stops after the attribution check. A detached release worktree has no branch for dev push, so the push goes through jj-git-guard. Exit 3 means the remote refused the push as non-fast-forward.
local dev_command = require("@dev/command")
local dev_os = require("@dev/os")
local fs = require("@dev/fs")
local forge = require("@lib/pomb/forge")

local words = { ... }

local FORBIDDEN = { "claude-session", "co-authored-by", "generated with", "claude.ai/code" }
local QUIET = { stderr = "null" }

local function usage()
    dev_command.note("usage: push_main.luau <worktree> [--check]")
    return 1
end

local function lines(text)
    local found = {}
    for line in text:gmatch("[^\n]+") do
        found[#found + 1] = line
    end
    return found
end

local function git(wt, ...)
    return { "git", "-C", wt, ... }
end

local function forbidden(line)
    local lower = line:lower()
    for _, word in ipairs(FORBIDDEN) do
        if lower:find(word, 1, true) then
            return true
        end
    end
    return false
end

local function change_id(line)
    return line:match("^Change%-Id: I[0-9a-f]+$") ~= nil and #line == 52
end

local function attribution(wt, range)
    local hits = {}
    local log = dev_os.capture(git(wt, "log", "--format=%B", range), QUIET)
    for _, line in ipairs(lines(log)) do
        if forbidden(line) then
            hits[#hits + 1] = line
        end
    end
    local commits = dev_os.capture(git(wt, "rev-list", range), QUIET)
    for _, commit in ipairs(lines(commits)) do
        local body = dev_os.capture(git(wt, "log", "-1", "--format=%B", commit), QUIET)
        local parsed = dev_os.capture_input(git(wt, "interpret-trailers", "--parse"), body, QUIET)
        for _, line in ipairs(lines(parsed)) do
            if not change_id(line) then
                hits[#hits + 1] = line
            end
        end
    end
    return hits
end

local function push(wt, remote)
    local sha = dev_os.capture(git(wt, "rev-parse", "HEAD"), QUIET):gsub("%s+$", "")
    local log = assert(fs.mktemp({ prefix = "push_main" }))
    local argv = { "jj-git-guard", "push", "--", remote, sha .. ":refs/heads/main" }
    local status = dev_os.run(argv, { cwd = wt, stderr = log })
    local text = fs.read(log) or ""
    fs.remove(log)
    dev_os.stderr:write(text)
    if status == 0 then
        return 0
    end
    if text:find("non-fast-forward", 1, true) or text:find("fetch first", 1, true) then
        return 3
    end
    return 1
end

local wt = words[1]
if wt == nil or wt == "" then
    return usage()
end
local mode = words[2] or "push"

local remote, err = forge.remote(wt)
if not remote then
    dev_command.note(err)
    return 1
end

local hits = attribution(wt, remote .. "/main..HEAD")
if #hits > 0 then
    dev_command.note(
        "The commit messages hold attribution or trailers, listed below.\n"
            .. table.concat(hits, "\n")
    )
    return 1
end
if mode == "--check" then
    return 0
end
return push(wt, remote)
