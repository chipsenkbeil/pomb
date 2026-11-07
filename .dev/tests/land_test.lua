local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

devos.defaults.env = {
    GIT_CONFIG_GLOBAL = "/dev/null",
    GIT_CONFIG_NOSYSTEM = "1",
    GIT_AUTHOR_NAME = "Test",
    GIT_AUTHOR_EMAIL = "test@example.com",
    GIT_COMMITTER_NAME = "Test",
    GIT_COMMITTER_EMAIL = "test@example.com",
}

local CARD = "abc123"
local RELEASE = "release"
local VERBS = { "land.lua", "__gate.lua", "__push_main.lua", "__push_tag.lua", "lua/pomb/forge.lua" }

local STUB_DEV = [[
case "$1" in __*) exec luajit '@main@' "$@";; esac
if [ "$1" = board ]; then echo "$*" >>'@dir@/notes'; exit 0; fi
touch '@dir@/runs.jsonl'
echo "$1" >>'@dir@/calls'
status=passed
code=0
if [ -e '@dir@/fail-'"$1" ]; then status=failed; code=1; fi
printf '{"run":"r%s","repo":"%s-0123456789abcdef","source":"local","status":"%s"}\n' "$(wc -l <'@dir@/runs.jsonl' | tr -d ' ')" "$(basename "$PWD")" "$status" >>'@dir@/runs.jsonl'
exit $code
]]

local STUB_CI = [[
touch '@dir@/runs.jsonl'
jq -s '{runs: .}' '@dir@/runs.jsonl'
]]

local STUB_JJ = [[
[ "$1" != root ] || exit 1
case " $* " in *" git remote list "*) echo "origin x"; exit 0;; *" git root "*) echo "@dir@/work/.git"; exit 0;; esac
echo "$*" >>'@dir@/jjcalls'
[ "$1" = --no-pager ] && shift
if [ "$1 $2" = 'workspace list' ]; then cat '@dir@/workspaces' 2>/dev/null; fi
if [ "$1 $2" = 'bookmark list' ]; then cat '@dir@/bookmarks' 2>/dev/null; fi
exit 0
]]

local STUB_GUARD = [[
echo "$*" >>'@dir@/pushes'
if [ -e '@dir@/fail-push' ]; then echo 'remote: denied' >&2; exit 1; fi
n=$(cat '@dir@/refuse' 2>/dev/null || echo 0)
if [ "$n" -gt 0 ]; then
echo $((n - 1)) >'@dir@/refuse'
g='@dir@/remote.git'
c=$(git --git-dir="$g" commit-tree -p main -m moved "$(git --git-dir="$g" rev-parse 'main^{tree}')")
git --git-dir="$g" update-ref refs/heads/main "$c"
echo '! [rejected] main -> main (fetch first)' >&2
exit 1
fi
shift 2
exec git push "$@"
]]

local function manifest(version)
    return ('[package]\nname = "pomb"\nversion = "%s"\n'):format(version)
end

local function lock(version)
    return ('[[package]]\nname = "pomb"\nversion = "%s"\n'):format(version)
end

local function git(dir, ...)
    local out, code = devos.capture({ "git", "-C", dir, ... })
    assert(code == 0, table.concat({ ... }, " "))
    return (out:gsub("%s+$", ""))
end

local function git_ok(dir, ...)
    return devos.run({ "git", "-C", dir, ... }, { stdout = "null", stderr = "null" }) == 0
end

local function launcher()
    local found = assert(package.searchpath("dev.cli", package.path))
    return (assert(fs.realpath(found)):gsub("/lua/dev/cli%.lua$", "/main.lua"))
end

local function stub(dir, name, body)
    local path = dir .. "/bin/" .. name
    local text = body:gsub("@(%w+)@", { dir = dir, main = launcher() })
    assert(fs.write(path, "#!/bin/sh\n" .. text))
    assert(devos.run({ "chmod", "755", path }) == 0)
end

local function fixture()
    local dir = assert(fs.realpath(testenv.tempdir()))
    local work = dir .. "/work"
    git(dir, "init", "-q", "--bare", "remote.git")
    git(dir, "clone", "-q", "remote.git", "work")
    git(work, "checkout", "-q", "-b", "main")
    assert(fs.write(work .. "/Cargo.toml", manifest("1.0.0")))
    assert(fs.write(work .. "/Cargo.lock", lock("1.0.0")))
    for _, verb in ipairs(VERBS) do
        local to = work .. "/.dev/" .. verb
        assert(fs.mkdir(fs.dirname(to), { parents = true }))
        assert(fs.write(to, assert(fs.read(ROOT .. "/.dev/" .. verb))))
    end
    git(work, "add", "-f", ".")
    git(work, "commit", "-q", "-m", "base")
    git(work, "push", "-q", "origin", "main")
    assert(fs.mkdir(dir .. "/bin"))
    stub(dir, "dev", STUB_DEV)
    stub(dir, "ci", STUB_CI)
    stub(dir, "jj", STUB_JJ)
    stub(dir, "jj-git-guard", STUB_GUARD)
    return { dir = dir, work = work, remote = dir .. "/remote.git" }
end

local function lines(fix, name)
    local text = fs.read(fix.dir .. "/" .. name) or ""
    local found = {}
    for line in text:gmatch("[^\n]+") do
        found[#found + 1] = line
    end
    return found
end

local function refuse_pushes(fix, count)
    assert(fs.write(fix.dir .. "/refuse", tostring(count)))
end

local function mark(fix, name)
    assert(fs.write(fix.dir .. "/" .. name, ""))
end

local function candidate(fix, files, message)
    git(fix.work, "checkout", "-q", "-B", "card", "origin/main")
    for name, body in pairs(files) do
        assert(fs.write(fix.work .. "/" .. name, body))
        git(fix.work, "add", "--", name)
    end
    git(fix.work, "commit", "-q", "-m", message)
    local sha = git(fix.work, "rev-parse", "HEAD")
    git(fix.work, "checkout", "-q", "main")
    return sha
end

local function advance_remote_main(fix, name)
    git(fix.dir, "clone", "-q", "-b", "main", "remote.git", "other")
    local other = fix.dir .. "/other"
    assert(fs.write(other .. "/" .. name, "x"))
    git(other, "add", ".")
    git(other, "commit", "-q", "-m", "main moved on")
    git(other, "push", "-q", "origin", "HEAD:refs/heads/main")
end

local function land(fix, commit)
    return testenv.verb(fix.work, { "land", commit, "--card", CARD }, {
        env = { TMPDIR = fix.dir, PATH = fix.dir .. "/bin:" .. os.getenv("PATH") },
    })
end

local function remote_main(fix)
    return git(fix.remote, "rev-parse", "refs/heads/main")
end

local function show(fix, spec)
    return git(fix.remote, "show", spec)
end

local function card_worktree(fix, name, branch)
    local path = fix.work .. "/.dev/worktrees/" .. name
    git(fix.work, "worktree", "add", "-q", path, branch)
    return path
end

local function jj_repo(fix, workspaces, bookmarks)
    assert(fs.mkdir(fix.work .. "/.jj"))
    local rows = {}
    for _, row in ipairs(workspaces) do
        rows[#rows + 1] = row[1] .. " " .. row[2] .. "\n"
    end
    assert(fs.write(fix.dir .. "/workspaces", table.concat(rows)))
    assert(fs.write(fix.dir .. "/bookmarks", table.concat(bookmarks, "\n") .. "\n"))
end

local function refused_candidate(refusals)
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    refuse_pushes(fix, refusals)
    return fix, cand
end

local function done(fix)
    fs.remove(fix.dir, { recursive = true })
end

local function eq_list(got, want)
    t.eq(table.concat(got, ","), table.concat(want, ","))
end

local function contains(list, value)
    for _, item in ipairs(list) do
        if item == value then
            return true
        end
    end
    return false
end

t.test("land pushes the commit onto current main after lint and test pass", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    advance_remote_main(fix, "other.txt")
    local moved = remote_main(fix)
    local out, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.eq(git(fix.remote, "rev-parse", "refs/heads/main^"), moved)
    t.eq(show(fix, "refs/heads/main:feature.txt"), "feature")
    t.eq(show(fix, "refs/heads/main:other.txt"), "x")
    eq_list(lines(fix, "calls"), { "lint", "test" })
    t.ok(out:find("Landed " .. remote_main(fix) .. ".", 1, true), out)
    t.ok(not fs.exists(fix.work .. "/.dev/worktrees/land-" .. CARD))
    done(fix)
end)

t.test("land runs lint before test and stops at the first failure", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    mark(fix, "fail-lint")
    local before = remote_main(fix)
    local _, code = land(fix, cand)
    t.ok(code ~= 0)
    eq_list(lines(fix, "calls"), { "lint" })
    t.eq(remote_main(fix), before)
    done(fix)
end)

t.test("land refuses a commit with an attribution trailer before any gate", function()
    local fix = fixture()
    local cand =
        candidate(fix, { ["feature.txt"] = "feature" }, "add feature\n\nClaude-Session: abc123")
    local before = remote_main(fix)
    local _, code, err = land(fix, cand)
    t.ok(code ~= 0)
    t.ok(err:find("Claude-Session", 1, true), err)
    t.eq(#lines(fix, "calls"), 0)
    t.eq(remote_main(fix), before)
    done(fix)
end)

t.test("land refuses a commit that bumps the package version", function()
    local fix = fixture()
    local cand = candidate(fix, {
        ["Cargo.toml"] = manifest("1.0.1"),
        ["Cargo.lock"] = lock("1.0.1"),
    }, "bump the version")
    local before = remote_main(fix)
    local _, code, err = land(fix, cand)
    t.ok(code ~= 0)
    t.ok(err:find("changes the package version", 1, true), err)
    t.eq(#lines(fix, "calls"), 0)
    t.eq(remote_main(fix), before)
    done(fix)
end)

t.test("land does not push when a gate fails", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    mark(fix, "fail-test")
    local before = remote_main(fix)
    local _, code = land(fix, cand)
    t.ok(code ~= 0)
    eq_list(lines(fix, "calls"), { "lint", "test" })
    t.eq(remote_main(fix), before)
    t.eq(#lines(fix, "notes"), 0)
    done(fix)
end)

t.test("land refuses a commit that is more than one commit past main", function()
    local fix = fixture()
    candidate(fix, { ["first.txt"] = "first" }, "add first")
    git(fix.work, "checkout", "-q", "card")
    assert(fs.write(fix.work .. "/second.txt", "second"))
    git(fix.work, "add", "--", "second.txt")
    git(fix.work, "commit", "-q", "-m", "add second")
    local cand = git(fix.work, "rev-parse", "HEAD")
    git(fix.work, "checkout", "-q", "main")
    local before = remote_main(fix)
    local _, code, err = land(fix, cand)
    t.ok(code ~= 0)
    t.ok(err:find("expected exactly one", 1, true), err)
    t.eq(#lines(fix, "calls"), 0)
    t.eq(remote_main(fix), before)
    done(fix)
end)

t.test("land reapplies and reruns both gates when the first push is refused", function()
    local fix, cand = refused_candidate(1)
    local _, code, err = land(fix, cand)
    t.ok(code == 0, err)
    eq_list(lines(fix, "calls"), { "lint", "test", "lint", "test" })
    t.eq(#lines(fix, "pushes"), 2)
    t.eq(show(fix, "refs/heads/main:feature.txt"), "feature")
    t.ok(show(fix, "refs/heads/main^"):find("moved", 1, true))
    t.ok(not fs.exists(fix.work .. "/.dev/worktrees/land-" .. CARD))
    done(fix)
end)

t.test("land gives up after three refused pushes without forcing", function()
    local fix, cand = refused_candidate(3)
    local _, code, err = land(fix, cand)
    t.ok(code ~= 0)
    t.ok(err:find("moved on 3 times", 1, true), err)
    t.eq(#lines(fix, "calls"), 6)
    t.eq(#lines(fix, "pushes"), 3)
    t.eq(#lines(fix, "notes"), 0)
    local tree = git(fix.remote, "ls-tree", "--name-only", "refs/heads/main")
    t.ok(not tree:find("feature.txt", 1, true))
    done(fix)
end)

t.test("land never passes a force flag to the push", function()
    local fix, cand = refused_candidate(2)
    local _, code, err = land(fix, cand)
    t.ok(code == 0, err)
    local pushes = lines(fix, "pushes")
    t.eq(#pushes, 3)
    for _, push in ipairs(pushes) do
        for arg in push:gmatch("%S+") do
            t.ok(arg:sub(1, 1) ~= "+" and not arg:find("force", 1, true) and arg ~= "-f", push)
        end
    end
    done(fix)
end)

t.test("land notes the landed sha on the card", function()
    local fix, cand = refused_candidate(1)
    local _, code, err = land(fix, cand)
    t.ok(code == 0, err)
    local notes = lines(fix, "notes")
    t.eq(#notes, 2, table.concat(notes, "|"))
    t.eq(notes[1], "board note " .. CARD .. " landed " .. remote_main(fix) .. " on origin/main")
    t.eq(notes[2], "board move " .. CARD .. " " .. RELEASE)
    done(fix)
end)

t.test("land removes the card worktree and deletes its branch", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    local worktree = card_worktree(fix, "feat", "card")
    local _, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.ok(not fs.exists(worktree))
    t.ok(not git_ok(fix.work, "rev-parse", "--verify", "-q", "refs/heads/card"))
    eq_list(lines(fix, "calls"), { "lint", "test" })
    t.eq(lines(fix, "notes")[2], "board move " .. CARD .. " " .. RELEASE)
    t.eq(#lines(fix, "jjcalls"), 0)
    done(fix)
end)

t.test("land forgets the jj workspace and deletes the bookmark of the card worktree", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    local worktree = card_worktree(fix, "feat", "card")
    jj_repo(fix, { { "default", fix.work }, { "feat", worktree } }, { "main", "card" })
    local _, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.ok(not fs.exists(worktree))
    t.ok(not git_ok(fix.work, "rev-parse", "--verify", "-q", "refs/heads/card"))
    local jj = lines(fix, "jjcalls")
    local text = table.concat(jj, "\n")
    t.ok(contains(jj, "--no-pager workspace forget feat"), text)
    t.ok(not text:find("forget default", 1, true), text)
    t.ok(contains(jj, "--no-pager bookmark delete card"), text)
    t.ok(not text:find("bookmark delete main", 1, true), text)
    done(fix)
end)

t.test("land keeps the worktrees it did not land from", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    git(fix.work, "branch", "-q", "other", "origin/main")
    git(fix.work, "branch", "-q", "outside", "card")
    local landed = card_worktree(fix, "feat", "card")
    local other = card_worktree(fix, "other", "other")
    local detached = fix.work .. "/.dev/worktrees/detached"
    git(fix.work, "worktree", "add", "-q", "--detach", detached, cand)
    local outside = fix.dir .. "/outside"
    git(fix.work, "worktree", "add", "-q", outside, "outside")
    local _, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.ok(not fs.exists(landed))
    t.ok(not git_ok(fix.work, "rev-parse", "--verify", "-q", "refs/heads/card"))
    t.ok(fs.exists(other))
    t.ok(git_ok(fix.work, "rev-parse", "--verify", "-q", "refs/heads/other"))
    t.ok(fs.exists(detached))
    t.ok(fs.exists(outside))
    t.ok(git_ok(fix.work, "rev-parse", "--verify", "-q", "refs/heads/outside"))
    done(fix)
end)

t.test("land keeps a card worktree with uncommitted changes", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    local worktree = card_worktree(fix, "feat", "card")
    assert(fs.write(worktree .. "/scratch.txt", "wip"))
    local out, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.ok(fs.exists(worktree))
    t.ok(git_ok(fix.work, "rev-parse", "--verify", "-q", "refs/heads/card"))
    t.ok(out:find("uncommitted changes", 1, true), out)
    done(fix)
end)

t.test("land leaves the card worktree and column alone when the push is denied", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    local worktree = card_worktree(fix, "feat", "card")
    mark(fix, "fail-push")
    local _, code = land(fix, cand)
    t.ok(code ~= 0)
    t.eq(#lines(fix, "notes"), 0)
    t.ok(fs.exists(worktree))
    t.ok(git_ok(fix.work, "rev-parse", "--verify", "-q", "refs/heads/card"))
    done(fix)
end)

t.test("land does not retry on an exit one push failure", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    mark(fix, "fail-push")
    local before = remote_main(fix)
    local _, code = land(fix, cand)
    t.ok(code ~= 0)
    eq_list(lines(fix, "calls"), { "lint", "test" })
    t.eq(#lines(fix, "pushes"), 1)
    t.eq(#lines(fix, "notes"), 0)
    t.eq(remote_main(fix), before)
    done(fix)
end)
