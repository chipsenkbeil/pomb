local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")
local gitenv = require("pomb.gitenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

devos.defaults.env = gitenv.env

local CARD = "abc123"
local RELEASE = "release"
local VERBS = {
    "land.lua",
    "__gate.lua",
    "__push_main.lua",
    "__push_tag.lua",
    "lua/pomb/cargo.lua",
    "lua/pomb/drain.lua",
    "lua/pomb/forge.lua",
    "lua/pomb/lock.lua",
    "lua/pomb/vcs.lua",
}

local STUB_DEV = [[
case "$1" in __*) exec luajit '@main@' "$@";; esac
if [ "$1" = board ] && [ "$2" = show ]; then [ -e '@dir@/fail-show' ] && exit 1; printf 'abc123  Ready  1 Pomodoro  %s  name\n' "$(cat '@dir@/labels' 2>/dev/null || echo -)"; cat '@dir@/check-note' 2>/dev/null; exit 0; fi
if [ "$1" = board ]; then echo "$*" >>'@dir@/notes'; exit 0; fi
touch '@dir@/runs.jsonl'
echo "$1" >>'@dir@/calls'
if [ -e '@dir@/drain-'"$1" ] && [ "$(cat '@dir@/drain-'"$1")" -gt 0 ]; then
echo $(($(cat '@dir@/drain-'"$1") - 1)) >'@dir@/drain-'"$1"
echo 'dev: the hub is draining and takes no new work'
exit 1
fi
if [ "$1" = lint ] && [ -e '@dir@/lock-on-lint' ]; then
printf 'batch=9 pid=%s\n' "$(cat '@dir@/lock-on-lint')" >'@dir@/work/.dev/release.lock'
rm '@dir@/lock-on-lint'
fi
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

local git = gitenv.git

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
    return { dir = dir, work = work, remote = dir .. "/remote.git", name = "origin" }
end

local function lines(fix, name)
    local text = fs.read(fix.dir .. "/" .. name) or ""
    local found = {}
    for line in text:gmatch("[^\n]+") do
        found[#found + 1] = line
    end
    return found
end

local function check_note(fix, commit, rows, mode)
    local text = {
        ("2d2adabc  agents  status  -  2026-11-09 18:58  dev-check: commit=%s change=- mode=%s\n"):format(
            commit,
            mode or "run"
        ),
    }
    for i, row in ipairs(rows) do
        text[#text + 1] = ("%s%d %s - %s\n"):format((" "):rep(40), i, row[1], row[2])
    end
    assert(fs.write(fix.dir .. "/check-note", table.concat(text)))
end

local PASSED = { { "pass", "dev --remote lint" }, { "pass", "dev --remote test" } }

local function refuse_pushes(fix, count)
    assert(fs.write(fix.dir .. "/refuse", tostring(count)))
end

local function mark(fix, name)
    assert(fs.write(fix.dir .. "/" .. name, ""))
end

local function candidate(fix, files, message)
    git(fix.work, "checkout", "-q", "-B", "card", fix.name .. "/main")
    for name, body in pairs(files) do
        assert(fs.mkdir(fs.dirname(fix.work .. "/" .. name), { parents = true }))
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

local function land(fix, commit, env)
    local vars = { TMPDIR = fix.dir, PATH = fix.dir .. "/bin:" .. os.getenv("PATH") }
    for name, value in pairs(env or {}) do
        vars[name] = value
    end
    return testenv.verb(fix.work, { "land", commit, "--card", CARD }, { env = vars })
end

local STUB_SLEEP = [=[
echo "$*" >>'@dir@/sleeps'
[ -e '@dir@/keep-lock' ] || rm -f '@dir@/work/.dev/release.lock'
]=]

local function lock_path(fix)
    return fix.work .. "/.dev/release.lock"
end

local function hold_lock(fix, pid)
    stub(fix.dir, "sleep", STUB_SLEEP)
    assert(fs.write(lock_path(fix), ("batch=9 pid=%d\n"):format(pid)))
end

local function dead_pid()
    local out = devos.capture({ "sh", "-c", "echo $$" })
    return tonumber(out)
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

local function candidate_with_refused_pushes(refusals)
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    refuse_pushes(fix, refusals)
    return fix, cand
end

local function done(fix)
    fs.remove(fix.dir, { recursive = true })
end

local function gerrit_fixture()
    local fix = fixture()
    fix.name = "gerrit"
    git(fix.work, "remote", "rename", "origin", fix.name)
    return fix
end

local function change_ids(fix)
    local body = git(fix.remote, "log", "-1", "--format=%B", "refs/heads/main")
    local found = {}
    for id in body:gmatch("Change%-Id: (I%x+)") do
        found[#found + 1] = id
    end
    return found
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

t.test("land skips the gates when the card's check passed on the landed commit", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    check_note(fix, cand, PASSED)
    local out, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.eq(#lines(fix, "calls"), 0)
    t.eq(show(fix, "refs/heads/main:feature.txt"), "feature")
    t.ok(out:find("The gates are skipped because", 1, true), out)
    t.ok(out:find("on commit " .. cand, 1, true), out)
    done(fix)
end)

t.test("land skips the gates on a moved main when the patch-id matches", function()
    local fix = fixture()
    local checked = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    advance_remote_main(fix, "other.txt")
    git(fix.work, "fetch", "-q", fix.name)
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    t.ok(cand ~= checked)
    check_note(fix, checked, PASSED)
    local out, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.eq(#lines(fix, "calls"), 0)
    t.eq(show(fix, "refs/heads/main:other.txt"), "x")
    t.eq(show(fix, "refs/heads/main:feature.txt"), "feature")
    t.ok(out:find("same patch-id", 1, true), out)
    done(fix)
end)

t.test("land skips the gates on a race retry", function()
    local fix, cand = candidate_with_refused_pushes(1)
    check_note(fix, cand, PASSED)
    local out, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.eq(#lines(fix, "calls"), 0)
    t.eq(#lines(fix, "pushes"), 2)
    t.eq(show(fix, "refs/heads/main:feature.txt"), "feature")
    local _, skips = out:gsub("The gates are skipped because", "")
    t.eq(skips, 2)
    done(fix)
end)

t.test("land still refuses attribution when the gates are skipped", function()
    local fix = fixture()
    local cand =
        candidate(fix, { ["feature.txt"] = "feature" }, "add feature\n\nClaude-Session: abc123")
    check_note(fix, cand, PASSED)
    local before = remote_main(fix)
    local _, code, err = land(fix, cand)
    t.ok(code ~= 0)
    t.ok(err:find("Claude-Session", 1, true), err)
    t.eq(remote_main(fix), before)
    done(fix)
end)

local function gated(name, setup, reason)
    t.test("land gates as today " .. name, function()
        local fix = fixture()
        local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
        setup(fix, cand)
        local out, code, err = land(fix, cand)
        t.ok(code == 0, err)
        eq_list(lines(fix, "calls"), { "lint", "test" })
        t.ok(out:find("The gates run because " .. reason, 1, true), out)
        done(fix)
    end)
end

gated("without a check note", function() end, "the card has no dev-check note")

gated("when a check row failed", function(fix, cand)
    check_note(fix, cand, { { "pass", "dev --remote lint" }, { "fail", "dev --remote test" } })
end, "its latest dev-check has no pass for dev --remote test")

gated("when a check row is missing", function(fix, cand)
    check_note(fix, cand, { { "pass", "dev --remote lint" } })
end, "its latest dev-check has no pass for dev --remote test")

gated("when the check was not a run", function(fix, cand)
    check_note(fix, cand, PASSED, "expect-fail")
end, "its latest dev-check has mode expect-fail")

gated("when the check names a different patch", function(fix)
    local other = candidate(fix, { ["other.txt"] = "other" }, "add other")
    check_note(fix, other, PASSED)
end, "its latest dev-check commit ")

gated("when the check names a commit that cannot be read", function(fix)
    check_note(fix, ("0"):rep(40), PASSED)
end, "its latest dev-check commit ")

t.test("land_should_run_nixcheck_for_a_change_under_tests_or_dev", function()
    for _, path in ipairs({ "tests/a.txt", ".dev/b.txt", "extra/Cargo.toml" }) do
        local fix = fixture()
        local cand = candidate(fix, { [path] = "x" }, "touch " .. path)
        local _, code, err = land(fix, cand)
        t.ok(code == 0, err)
        eq_list(lines(fix, "calls"), { "lint", "test", "nixcheck" })
        done(fix)
    end
    local fix = fixture()
    local cand = candidate(fix, { ["tests/a.txt"] = "x" }, "add a test")
    mark(fix, "fail-nixcheck")
    local before = remote_main(fix)
    local _, code = land(fix, cand)
    t.ok(code ~= 0)
    eq_list(lines(fix, "calls"), { "lint", "test", "nixcheck" })
    t.eq(remote_main(fix), before)
    done(fix)
    fix = fixture()
    cand = candidate(fix, { [".dev/b.txt"] = "x" }, "touch dev")
    check_note(fix, cand, PASSED)
    local out
    out, code = land(fix, cand)
    t.ok(code == 0)
    eq_list(lines(fix, "calls"), { "nixcheck" })
    t.ok(out:find("The gates are skipped because", 1, true), out)
    done(fix)
end)

t.test("land_should_skip_nixcheck_for_a_change_to_src", function()
    local fix = fixture()
    local cand = candidate(fix, { ["src/lib.rs"] = "x" }, "touch src")
    local _, code, err = land(fix, cand)
    t.ok(code == 0, err)
    eq_list(lines(fix, "calls"), { "lint", "test" })
    done(fix)
end)

t.test("land refuses a standalone script without the standalone-ok label", function()
    local fix = fixture()
    local cand =
        candidate(fix, { [".dev/standalone/x.sh"] = "#!/bin/sh\n" }, "add a standalone script")
    local before = remote_main(fix)
    local _, code, err = land(fix, cand)
    t.ok(code ~= 0)
    t.ok(err:find(".dev/standalone/x.sh", 1, true), err)
    t.ok(err:find("standalone-ok", 1, true), err)
    t.eq(#lines(fix, "calls"), 0)
    t.eq(remote_main(fix), before)
    done(fix)
end)

t.test("land lets a standalone script through with the standalone-ok label", function()
    local fix = fixture()
    assert(fs.write(fix.dir .. "/labels", "docs,standalone-ok"))
    local cand =
        candidate(fix, { [".dev/standalone/x.sh"] = "#!/bin/sh\n" }, "add a standalone script")
    local _, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.eq(show(fix, "refs/heads/main:.dev/standalone/x.sh"), "#!/bin/sh")
    done(fix)
end)

t.test("land refuses a standalone script whose name needs quoting", function()
    local fix = fixture()
    local name = ".dev/standalone/caf\195\169 \"x\".sh"
    local cand = candidate(fix, { [name] = "#!/bin/sh\n" }, "add a standalone script")
    local before = remote_main(fix)
    local _, code, err = land(fix, cand)
    t.ok(code ~= 0)
    t.ok(err:find("standalone-ok", 1, true), err)
    t.eq(#lines(fix, "calls"), 0)
    t.eq(remote_main(fix), before)
    done(fix)
end)

t.test("land refuses a standalone script when the card cannot be read", function()
    local fix = fixture()
    local cand =
        candidate(fix, { [".dev/standalone/x.sh"] = "#!/bin/sh\n" }, "add a standalone script")
    local before = remote_main(fix)
    mark(fix, "fail-show")
    local _, code = land(fix, cand)
    t.ok(code ~= 0)
    t.eq(#lines(fix, "calls"), 0)
    t.eq(remote_main(fix), before)
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
    local fix, cand = candidate_with_refused_pushes(1)
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
    local fix, cand = candidate_with_refused_pushes(3)
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
    local fix, cand = candidate_with_refused_pushes(2)
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
    local fix, cand = candidate_with_refused_pushes(1)
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

t.test("land removes a clean land worktree left by an aborted attempt", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    local stale = fix.work .. "/.dev/worktrees/land-" .. CARD
    git(fix.work, "worktree", "add", "-q", "--detach", stale, "origin/main")
    local out, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.ok(not fs.exists(stale))
    t.eq(show(fix, "refs/heads/main:feature.txt"), "feature")
    t.ok(out:find("Landed " .. remote_main(fix) .. ".", 1, true), out)
    done(fix)
end)

t.test("land refuses a land worktree with uncommitted changes and keeps it", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    local stale = fix.work .. "/.dev/worktrees/land-" .. CARD
    git(fix.work, "worktree", "add", "-q", "--detach", stale, "origin/main")
    assert(fs.write(stale .. "/scratch.txt", "wip"))
    local before = remote_main(fix)
    local _, code, err = land(fix, cand)
    t.ok(code ~= 0)
    t.ok(err:find(".dev/worktrees/land-" .. CARD, 1, true), err)
    t.ok(err:find("uncommitted changes", 1, true), err)
    t.ok(fs.exists(stale))
    t.eq(remote_main(fix), before)
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

t.test("land should wait for the release lock before applying", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    hold_lock(fix, devos.getpid())
    local out, code, err = land(fix, cand, { POMB_LAND_WAIT_INTERVAL = "1" })
    t.ok(code == 0, err)
    t.ok(out:find("Waiting for the release of batch 9", 1, true), out)
    t.eq(#lines(fix, "sleeps"), 1)
    t.eq(show(fix, "refs/heads/main:feature.txt"), "feature")
    t.ok(not fs.exists(lock_path(fix)))
    done(fix)
end)

t.test("land should give up when the release lock outlasts the wait bound", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    hold_lock(fix, devos.getpid())
    mark(fix, "keep-lock")
    local before = remote_main(fix)
    local env = { POMB_LAND_WAIT_INTERVAL = "1", POMB_LAND_WAIT_BOUND = "2" }
    local _, code, err = land(fix, cand, env)
    t.ok(code ~= 0)
    t.ok(err:find("still held the land lock after 2 seconds", 1, true), err)
    t.eq(#lines(fix, "calls"), 0)
    t.eq(remote_main(fix), before)
    done(fix)
end)

t.test("land should take over a stale lock from a dead process", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    hold_lock(fix, dead_pid())
    local out, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.ok(out:find("is gone, so it is taken over", 1, true), out)
    t.eq(#lines(fix, "sleeps"), 0)
    t.ok(not fs.exists(lock_path(fix)))
    t.eq(show(fix, "refs/heads/main:feature.txt"), "feature")
    done(fix)
end)

t.test("land should apply again when a release takes the lock during its gates", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    stub(fix.dir, "sleep", STUB_SLEEP)
    assert(fs.write(fix.dir .. "/lock-on-lint", tostring(devos.getpid())))
    local out, code, err = land(fix, cand, { POMB_LAND_WAIT_INTERVAL = "1" })
    t.ok(code == 0, err)
    t.ok(out:find("A release took the land lock while the gates ran", 1, true), out)
    eq_list(lines(fix, "calls"), { "lint", "test", "lint", "test" })
    t.eq(#lines(fix, "pushes"), 1)
    t.eq(show(fix, "refs/heads/main:feature.txt"), "feature")
    done(fix)
end)

t.test("land should apply on the main a release pushed while it waited", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    git(fix.dir, "clone", "-q", "-b", "main", "remote.git", "other")
    assert(fs.write(fix.dir .. "/other/released.txt", "released"))
    git(fix.dir .. "/other", "add", ".")
    git(fix.dir .. "/other", "commit", "-q", "-m", "release batch 9")
    stub(
        fix.dir,
        "sleep",
        [=[
echo "$*" >>'@dir@/sleeps'
rm -f '@dir@/work/.dev/release.lock'
git -C '@dir@/other' push -q origin HEAD:refs/heads/main
]=]
    )
    assert(fs.write(lock_path(fix), ("batch=9 pid=%d\n"):format(devos.getpid())))
    local out, code, err = land(fix, cand, { POMB_LAND_WAIT_INTERVAL = "1" })
    t.ok(code == 0, err)
    t.ok(out:find("Waiting for the release of batch 9", 1, true), out)
    eq_list(lines(fix, "calls"), { "lint", "test" })
    t.eq(#lines(fix, "pushes"), 1)
    t.eq(show(fix, "refs/heads/main:released.txt"), "released")
    t.eq(show(fix, "refs/heads/main:feature.txt"), "feature")
    done(fix)
end)

t.test("land should add a change id on gerrit", function()
    local fix = gerrit_fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    local _, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.eq(#change_ids(fix), 1)
    done(fix)
end)

t.test("land should keep an existing change id on gerrit", function()
    local fix = gerrit_fixture()
    local id = "I0123456789abcdef0123456789abcdef01234567"
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature\n\nChange-Id: " .. id)
    local _, code, err = land(fix, cand)
    t.ok(code == 0, err)
    eq_list(change_ids(fix), { id })
    done(fix)
end)

t.test("land should carry one change id after a moved main retry on gerrit", function()
    local fix = gerrit_fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    refuse_pushes(fix, 1)
    local _, code, err = land(fix, cand)
    t.ok(code == 0, err)
    t.eq(#lines(fix, "pushes"), 2)
    t.eq(#change_ids(fix), 1)
    done(fix)
end)

t.test("land should retry a gate refused by a drain", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    stub(fix.dir, "sleep", STUB_SLEEP)
    assert(fs.write(fix.dir .. "/drain-lint", "1"))
    local env = { POMB_DRAIN_INTERVAL = "1", POMB_DRAIN_BOUND = "5" }
    local out, code, err = land(fix, cand, env)
    t.ok(code == 0, err)
    t.ok(out:find("The dev lint --remote run waits 1 seconds because the ci hub is draining.", 1, true), out)
    eq_list(lines(fix, "calls"), { "lint", "lint", "test" })
    t.eq(#lines(fix, "pushes"), 1)
    t.eq(show(fix, "refs/heads/main:feature.txt"), "feature")
    done(fix)
end)

t.test("land should abort naming the drain when it outlasts the bound", function()
    local fix = fixture()
    local cand = candidate(fix, { ["feature.txt"] = "feature" }, "add feature")
    stub(fix.dir, "sleep", STUB_SLEEP)
    assert(fs.write(fix.dir .. "/drain-lint", "99"))
    local before = remote_main(fix)
    local env = { POMB_DRAIN_INTERVAL = "1", POMB_DRAIN_BOUND = "2" }
    local _, code, err = land(fix, cand, env)
    t.ok(code ~= 0)
    t.ok(err:find("still draining after 2 seconds", 1, true), err)
    eq_list(lines(fix, "calls"), { "lint", "lint", "lint" })
    t.eq(#lines(fix, "pushes"), 0)
    t.eq(remote_main(fix), before)
    done(fix)
end)
