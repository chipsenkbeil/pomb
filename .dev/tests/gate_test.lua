local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")
local jjenv = require("pomb.jjenv")
local testkit = require("pomb.testkit")

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

---@class gate_test.Opts
---@field before string[] run rows before the gate runs
---@field after string[] run rows the dev stub leaves behind
---@field dev_exit integer exit code of the dev stub
---@field kind? string repository kind of the worktree

---@param opts gate_test.Opts
local function gate(opts)
    local before, after, dev_exit, kind = opts.before, opts.after, opts.dev_exit, opts.kind
    local root = assert(fs.realpath(testenv.tempdir()))
    assert(fs.mkdir(root .. "/bin"))
    assert(fs.mkdir(root .. "/" .. WORKTREE))
    assert(fs.symlink(testkit.root .. "/.dev", root .. "/.dev"))
    assert(fs.write(root .. "/runs.json", doc(before)))
    assert(fs.write(root .. "/next.json", doc(after)))
    local real_git = (assert(devos.capture({ "sh", "-c", "command -v git" }))):gsub("%s+$", "")
    local wt = root .. "/" .. WORKTREE
    local git_head = HEAD
    if kind == jjenv.Kind.JJ then
        git_head = OTHER
        assert(fs.mkdir(wt .. "/.jj"))
        local words = [[[ "$1 $2 $3 $4 $5" = '--no-pager log --no-graph -r @-' ] && echo %s]]
        testkit.stub(root .. "/bin/jj", words:format(HEAD))
    end
    testkit.stub(
        root .. "/bin/git",
        ([[if [ "$1" = -C ] && [ "$2" = '%s' ] && [ "$3" = rev-parse ] && [ "$4" = HEAD ]; then
echo %s
exit 0
fi
exec '%s' "$@"]]):format(wt, git_head, real_git)
    )
    testkit.stub(root .. "/bin/ci", "cat '" .. root .. "/runs.json'")
    local copy = ("cp '%s/next.json' '%s/runs.json'\nexit %d"):format(root, root, dev_exit)
    testkit.stub(root .. "/bin/dev", copy)
    local words = { "__gate", root .. "/" .. WORKTREE, "lint", root .. "/gate.log" }
    local out, code, err = testenv.verb(root, words)
    fs.remove(root, { recursive = true })
    return out, code, err
end

t.test("gate accepts one new passed local run", function()
    local old = row("aaaaaa", "passed", "local")
    local out, code, err = gate({ before = { old }, after = { old, row("bbbbbb", "passed", "local") }, dev_exit = 0 })
    t.eq(code, 0, err)
    t.eq(out, "bbbbbb " .. REPO .. " local\n")
end)

t.test("gate ignores new runs of other repos", function()
    local after = {
        row("aaaaaa", "running", "local", "questions-0123456789abcdef"),
        row("bbbbbb", "passed", "local"),
        row("cccccc", "passed", "local", "release-10-0123456789abcdef"),
    }
    local out, code, err = gate({ before = {}, after = after, dev_exit = 0 })
    t.eq(code, 0, err)
    t.eq(out, "bbbbbb " .. REPO .. " local\n")
end)

t.test("gate refuses when no new run appears under the worktree", function()
    local runs = { row("aaaaaa", "passed", "local") }
    local _, code, err = gate({ before = runs, after = runs, dev_exit = 0 })
    t.ok(code ~= 0)
    t.ok(err:find("did not test this worktree", 1, true), err)
end)

t.test("gate refuses two new runs", function()
    local after = { row("aaaaaa", "passed", "local"), row("bbbbbb", "passed", "local") }
    local _, code = gate({ before = {}, after = after, dev_exit = 0 })
    t.ok(code ~= 0)
end)

t.test("gate refuses a run that did not pass", function()
    local _, code, err = gate({ before = {}, after = { row("aaaaaa", "failed", "local") }, dev_exit = 0 })
    t.ok(code ~= 0)
    t.ok(err:find("only a passed run counts.", 1, true), err)
end)

t.test("gate refuses a dirty source", function()
    local _, code, err = gate({ before = {}, after = { row("aaaaaa", "passed", "local(dirty)") }, dev_exit = 0 })
    t.ok(code ~= 0)
    t.ok(err:find("only a clean local source counts.", 1, true), err)
end)

t.test("gate_should_accept_a_local_source_naming_head", function()
    local source = "local@" .. HEAD:sub(1, 12)
    local out, code, err = gate({ before = {}, after = { row("bbbbbb", "passed", source) }, dev_exit = 0 })
    t.eq(code, 0, err)
    t.eq(out, "bbbbbb " .. REPO .. " " .. source .. "\n")
end)

t.test("gate_should_refuse_a_local_source_naming_another_commit", function()
    local _, code, err = gate({ before = {}, after = { row("bbbbbb", "passed", "local@" .. OTHER:sub(1, 12)) }, dev_exit = 0 })
    t.ok(code ~= 0)
    t.ok(err:find("which is not the HEAD " .. HEAD .. " of", 1, true), err)
end)

t.test("gate_should_refuse_a_dirty_local_source_naming_head", function()
    local source = "local(dirty)@" .. HEAD:sub(1, 12)
    local _, code, err = gate({ before = {}, after = { row("bbbbbb", "passed", source) }, dev_exit = 0 })
    t.ok(code ~= 0)
    t.ok(err:find("only a clean local source counts.", 1, true), err)
end)

t.test("gate refuses when dev fails", function()
    local _, code = gate({ before = {}, after = { row("aaaaaa", "passed", "local") }, dev_exit = 1 })
    t.ok(code ~= 0)
end)

t.test("gate_should_read_head_through_jj_when_the_worktree_has_a_jj_directory", function()
    local source = "local@" .. HEAD:sub(1, 12)
    local out, code, err = gate({ before = {}, after = { row("bbbbbb", "passed", source) }, dev_exit = 0, kind = jjenv.Kind.JJ })
    t.eq(code, 0, err)
    t.eq(out, "bbbbbb " .. REPO .. " " .. source .. "\n")
end)

t.test("gate_should_refuse_a_source_naming_the_git_head_in_a_jj_worktree", function()
    local source = "local@" .. OTHER:sub(1, 12)
    local _, code, err = gate({ before = {}, after = { row("bbbbbb", "passed", source) }, dev_exit = 0, kind = jjenv.Kind.JJ })
    t.ok(code ~= 0)
    t.ok(err:find("which is not the HEAD " .. HEAD .. " of", 1, true), err)
end)

t.test("gate_should_read_head_through_git_without_a_jj_directory", function()
    local source = "local@" .. HEAD:sub(1, 12)
    local _, code, err = gate({ before = {}, after = { row("bbbbbb", "passed", source) }, dev_exit = 0, kind = jjenv.Kind.GIT })
    t.eq(code, 0, err)
end)
