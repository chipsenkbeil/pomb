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
---@field narrow? string[] rows a listing under 2000 runs returns after the dev stub, default after
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
    assert(fs.write(root .. "/narrow.json", doc(before)))
    assert(fs.write(root .. "/narrow_next.json", doc(opts.narrow or after)))
    local list = [[if [ "$7" -lt 2000 ]; then cat '%s/narrow.json'; else cat '%s/runs.json'; fi]]
    testkit.stub(root .. "/bin/ci", list:format(root, root))
    local copy = ("cp '%s/next.json' '%s/runs.json'\ncp '%s/narrow_next.json' '%s/narrow.json'\nexit %d"):format(
        root,
        root,
        root,
        root,
        dev_exit
    )
    testkit.stub(root .. "/bin/dev", copy)
    local words = { "__gate", root .. "/" .. WORKTREE, "lint", root .. "/gate.log" }
    local out, code, err = testenv.verb(root, words)
    fs.remove(root, { recursive = true })
    return out, code, err
end

t.test("gate accepts one new passed local run", function()
    local old = row("aaaaaa", "passed", "local")
    local out, code, err =
        gate({ before = { old }, after = { old, row("bbbbbb", "passed", "local") }, dev_exit = 0 })
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

t.test("gate finds a run past the newest 200", function()
    local after = { row("bbbbbb", "passed", "local") }
    local out, code, err = gate({ before = {}, after = after, dev_exit = 0, narrow = {} })
    t.eq(code, 0, err)
    t.eq(out, "bbbbbb " .. REPO .. " local\n")
end)

t.test("gate refuses a run that did not pass", function()
    local _, code, err =
        gate({ before = {}, after = { row("aaaaaa", "failed", "local") }, dev_exit = 0 })
    t.ok(code ~= 0)
    t.ok(err:find("only a passed run counts.", 1, true), err)
end)

t.test("gate refuses a dirty source", function()
    local _, code, err =
        gate({ before = {}, after = { row("aaaaaa", "passed", "local(dirty)") }, dev_exit = 0 })
    t.ok(code ~= 0)
    t.ok(err:find("only a clean local source counts.", 1, true), err)
end)

t.test("gate accepts a local source naming head", function()
    local source = "local@" .. HEAD:sub(1, 12)
    local out, code, err =
        gate({ before = {}, after = { row("bbbbbb", "passed", source) }, dev_exit = 0 })
    t.eq(code, 0, err)
    t.eq(out, "bbbbbb " .. REPO .. " " .. source .. "\n")
end)

t.test("gate refuses a local source naming another commit", function()
    local _, code, err = gate({
        before = {},
        after = { row("bbbbbb", "passed", "local@" .. OTHER:sub(1, 12)) },
        dev_exit = 0,
    })
    t.ok(code ~= 0)
    t.ok(err:find("which is not the HEAD " .. HEAD .. " of", 1, true), err)
end)

t.test("gate refuses a dirty local source naming head", function()
    local source = "local(dirty)@" .. HEAD:sub(1, 12)
    local _, code, err =
        gate({ before = {}, after = { row("bbbbbb", "passed", source) }, dev_exit = 0 })
    t.ok(code ~= 0)
    t.ok(err:find("only a clean local source counts.", 1, true), err)
end)

t.test("gate refuses when dev fails", function()
    local _, code =
        gate({ before = {}, after = { row("aaaaaa", "passed", "local") }, dev_exit = 1 })
    t.ok(code ~= 0)
end)

t.test("gate reads head through jj when the worktree has a jj directory", function()
    local source = "local@" .. HEAD:sub(1, 12)
    local out, code, err = gate({
        before = {},
        after = { row("bbbbbb", "passed", source) },
        dev_exit = 0,
        kind = jjenv.Kind.JJ,
    })
    t.eq(code, 0, err)
    t.eq(out, "bbbbbb " .. REPO .. " " .. source .. "\n")
end)

t.test("gate refuses a source naming the git head in a jj worktree", function()
    local source = "local@" .. OTHER:sub(1, 12)
    local _, code, err = gate({
        before = {},
        after = { row("bbbbbb", "passed", source) },
        dev_exit = 0,
        kind = jjenv.Kind.JJ,
    })
    t.ok(code ~= 0)
    t.ok(err:find("which is not the HEAD " .. HEAD .. " of", 1, true), err)
end)

t.test("gate reads head through git without a jj directory", function()
    local source = "local@" .. HEAD:sub(1, 12)
    local _, code, err = gate({
        before = {},
        after = { row("bbbbbb", "passed", source) },
        dev_exit = 0,
        kind = jjenv.Kind.GIT,
    })
    t.eq(code, 0, err)
end)
