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
cat .ci-baseline/marker >>'@dir@/seen'
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
    assert(fs.mkdir(work .. "/bin"))
    local stub = work .. "/bin/ci"
    assert(fs.write(stub, "#!/bin/sh\n" .. STUB_CI:gsub("@dir@", dir)))
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
