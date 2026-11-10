local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

local WORKTREE = "release-1"
local REPO = "release-1-0123456789abcdef"
local HEAD = "0123456789abcdef0123456789abcdef01234567"
local OTHER = "89abcdef0123456789abcdef0123456789abcdef"

local function row(id, status, source, repo)
    return ('{"run":"%s","repo":"%s","source":"%s","kind":"run","status":"%s"}'):format(
        id,
        repo or REPO,
        source,
        status
    )
end

local function doc(rows)
    return '{"protocol":48,"runs":[' .. table.concat(rows, ",") .. "]}"
end

local function stub(root, name, body)
    local path = root .. "/bin/" .. name
    assert(fs.write(path, "#!/bin/sh\n" .. body .. "\n"))
    assert(devos.run({ "chmod", "755", path }) == 0)
end

local function gate(before, after, dev_exit, head)
    local root = assert(fs.realpath(testenv.tempdir()))
    assert(fs.mkdir(root .. "/bin"))
    assert(fs.mkdir(root .. "/" .. WORKTREE))
    assert(fs.symlink(ROOT .. "/.dev", root .. "/.dev"))
    assert(fs.write(root .. "/runs.json", doc(before)))
    assert(fs.write(root .. "/next.json", doc(after)))
    local real_git = (assert(devos.capture({ "sh", "-c", "command -v git" }))):gsub("%s+$", "")
    local wt = root .. "/" .. WORKTREE
    stub(
        root,
        "git",
        ([[if [ "$1" = -C ] && [ "$2" = '%s' ] && [ "$3" = rev-parse ] && [ "$4" = HEAD ]; then
echo %s
exit 0
fi
exec '%s' "$@"]]):format(wt, head or HEAD, real_git)
    )
    stub(root, "ci", "cat '" .. root .. "/runs.json'")
    stub(root, "dev", ("cp '%s/next.json' '%s/runs.json'\nexit %d"):format(root, root, dev_exit))
    local words = { "__gate", root .. "/" .. WORKTREE, "lint", root .. "/gate.log" }
    local out, code, err = testenv.verb(root, words)
    fs.remove(root, { recursive = true })
    return out, code, err
end

t.test("gate accepts one new passed local run", function()
    local old = row("aaaaaa", "passed", "local")
    local out, code, err = gate({ old }, { old, row("bbbbbb", "passed", "local") }, 0)
    t.eq(code, 0, err)
    t.eq(out, "bbbbbb " .. REPO .. " local\n")
end)

t.test("gate ignores new runs of other repos", function()
    local after = {
        row("aaaaaa", "running", "local", "questions-0123456789abcdef"),
        row("bbbbbb", "passed", "local"),
        row("cccccc", "passed", "local", "release-10-0123456789abcdef"),
    }
    local out, code, err = gate({}, after, 0)
    t.eq(code, 0, err)
    t.eq(out, "bbbbbb " .. REPO .. " local\n")
end)

t.test("gate refuses when no new run appears under the worktree", function()
    local runs = { row("aaaaaa", "passed", "local") }
    local _, code, err = gate(runs, runs, 0)
    t.ok(code ~= 0)
    t.ok(err:find("did not test this worktree", 1, true), err)
end)

t.test("gate refuses two new runs", function()
    local after = { row("aaaaaa", "passed", "local"), row("bbbbbb", "passed", "local") }
    local _, code = gate({}, after, 0)
    t.ok(code ~= 0)
end)

t.test("gate refuses a run that did not pass", function()
    local _, code, err = gate({}, { row("aaaaaa", "failed", "local") }, 0)
    t.ok(code ~= 0)
    t.ok(err:find("only a passed run counts.", 1, true), err)
end)

t.test("gate refuses a dirty source", function()
    local _, code, err = gate({}, { row("aaaaaa", "passed", "local(dirty)") }, 0)
    t.ok(code ~= 0)
    t.ok(err:find("only a clean local source counts.", 1, true), err)
end)

t.test("gate_should_accept_a_local_source_naming_head", function()
    local source = "local@" .. HEAD:sub(1, 12)
    local out, code, err = gate({}, { row("bbbbbb", "passed", source) }, 0)
    t.eq(code, 0, err)
    t.eq(out, "bbbbbb " .. REPO .. " " .. source .. "\n")
end)

t.test("gate_should_refuse_a_local_source_naming_another_commit", function()
    local _, code, err = gate({}, { row("bbbbbb", "passed", "local@" .. OTHER:sub(1, 12)) }, 0)
    t.ok(code ~= 0)
    t.ok(err:find("which is not the HEAD " .. HEAD .. " of", 1, true), err)
end)

t.test("gate_should_refuse_a_dirty_local_source_naming_head", function()
    local source = "local(dirty)@" .. HEAD:sub(1, 12)
    local _, code, err = gate({}, { row("bbbbbb", "passed", source) }, 0)
    t.ok(code ~= 0)
    t.ok(err:find("only a clean local source counts.", 1, true), err)
end)

t.test("gate refuses when dev fails", function()
    local _, code = gate({}, { row("aaaaaa", "passed", "local") }, 1)
    t.ok(code ~= 0)
end)
