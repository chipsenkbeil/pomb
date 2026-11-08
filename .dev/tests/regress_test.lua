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

local STUB_CI = [[
if [ "$1" = -C ]; then cd "$2" || exit 2; fi
cat .ci-baseline/marker >>'@dir@/seen'
pwd -P >'@dir@/where'
cat marker >'@dir@/staged_marker'
cat extra >'@dir@/staged_extra'
if [ -e ignored ]; then echo yes >'@dir@/staged_ignored'; fi
git -C '@work@' status --porcelain >'@dir@/status_during'
if [ -e '@dir@/fail' ]; then exit 1; fi
exit 0
]]

local function git(dir, ...)
    local out, code = devos.capture({ "git", "-C", dir, ... })
    assert(code == 0, table.concat({ ... }, " "))
    return (out:gsub("%s+$", ""))
end

local function commit(work, body)
    assert(fs.write(work .. "/marker", body .. "\n"))
    git(work, "add", "marker")
    git(work, "commit", "-q", "-m", body)
    return git(work, "rev-parse", "HEAD")
end

local function fixture()
    local dir = assert(fs.realpath(testenv.tempdir()))
    local work = dir .. "/work"
    git(dir, "init", "-q", "--bare", "remote.git")
    git(dir, "clone", "-q", "remote.git", "work")
    git(work, "remote", "rename", "origin", "gerrit")
    git(work, "checkout", "-q", "-b", "main")
    assert(fs.symlink(ROOT .. "/.dev", work .. "/.dev"))
    local base = commit(work, "base")
    git(work, "push", "-q", "gerrit", "main")
    git(work, "checkout", "-q", "-b", "card")
    commit(work, "card")
    git(work, "checkout", "-q", "main")
    local moved = commit(work, "moved")
    git(work, "push", "-q", "gerrit", "main")
    git(work, "checkout", "-q", "card")
    assert(fs.write(work .. "/.gitignore", "ignored\n"))
    assert(fs.write(work .. "/ignored", "x\n"))
    assert(fs.write(work .. "/extra", "extra\n"))
    assert(fs.mkdir(work .. "/bin"))
    local stub = work .. "/bin/ci"
    local body = STUB_CI:gsub("@dir@", dir):gsub("@work@", work)
    assert(fs.write(stub, "#!/bin/sh\n" .. body))
    assert(devos.run({ "chmod", "755", stub }) == 0)
    return { dir = dir, work = work, base = base, moved = moved }
end

local function seen(fix)
    return fs.read(fix.dir .. "/seen")
end

t.test("regress exports the merge-base when the candidate is behind main", function()
    local fix = fixture()
    local _, code, err = testenv.verb(fix.work, { "regress" })
    t.eq(code, 0, err)
    t.eq(seen(fix), "base\n")
end)

t.test("regress prints the full baseline sha for the default", function()
    local fix = fixture()
    local _, code, err = testenv.verb(fix.work, { "regress" })
    t.eq(code, 0, err)
    t.ok(err:find("baseline: " .. fix.base .. "\n", 1, true), err)
end)

t.test("regress prints the full baseline sha for an explicit REV", function()
    local fix = fixture()
    local _, code, err = testenv.verb(fix.work, { "regress", "gerrit/main" })
    t.eq(code, 0, err)
    t.ok(err:find("baseline: " .. fix.moved .. "\n", 1, true), err)
end)

t.test("regress uses an explicit REV unchanged", function()
    local fix = fixture()
    local _, code, err = testenv.verb(fix.work, { "regress", "gerrit/main" })
    t.eq(code, 0, err)
    t.eq(seen(fix), "moved\n")
end)

t.test("regress leaves the working tree untouched", function()
    local fix = fixture()
    local before = git(fix.work, "status", "--porcelain")
    local _, code, err = testenv.verb(fix.work, { "regress" })
    t.eq(code, 0, err)
    t.eq((fs.read(fix.dir .. "/status_during"):gsub("%s+$", "")), before)
    t.eq(git(fix.work, "status", "--porcelain"), before)
    t.ok(not fs.exists(fix.work .. "/.ci-baseline"))
end)

t.test("regress ships the baseline inside the staged tree", function()
    local fix = fixture()
    local _, code, err = testenv.verb(fix.work, { "regress" })
    t.eq(code, 0, err)
    t.ok(fs.read(fix.dir .. "/where") ~= fix.work)
    t.eq(seen(fix), "base\n")
    t.eq(fs.read(fix.dir .. "/staged_marker"), "card\n")
    t.eq(fs.read(fix.dir .. "/staged_extra"), "extra\n")
    t.ok(not fs.exists(fix.dir .. "/staged_ignored"))
end)

t.test("regress removes the staged tree after a passing run", function()
    local fix = fixture()
    local _, code, err = testenv.verb(fix.work, { "regress" })
    t.eq(code, 0, err)
    t.ok(not fs.exists((fs.read(fix.dir .. "/where"):gsub("%s+$", ""))))
end)

t.test("regress removes the staged tree after a failing run", function()
    local fix = fixture()
    assert(fs.write(fix.dir .. "/fail", ""))
    local _, code = testenv.verb(fix.work, { "regress" })
    t.ok(code ~= 0)
    t.ok(not fs.exists((fs.read(fix.dir .. "/where"):gsub("%s+$", ""))))
end)
