local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")
local gitenv = require("pomb.gitenv")
local jjenv = require("pomb.jjenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

devos.defaults.env = gitenv.env

local case = jjenv.case
local Kind = jjenv.Kind

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

local git = gitenv.git

local function commit(work, body)
    assert(fs.write(work .. "/marker", body .. "\n"))
    git(work, "add", "marker")
    git(work, "commit", "-q", "-m", body)
    return git(work, "rev-parse", "HEAD")
end

local function fixture(kind)
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
    if kind == Kind.JJ then
        jjenv.install(work)
    end
    return { dir = dir, work = work, base = base, moved = moved }
end

local function seen(fix)
    return fs.read(fix.dir .. "/seen")
end

case("regress exports the merge-base when the candidate is behind main", function(kind)
    local fix = fixture(kind)
    local _, code, err = testenv.verb(fix.work, { "regress" })
    t.eq(code, 0, err)
    t.eq(seen(fix), "base\n")
end)

case("regress prints the full baseline sha for the default", function(kind)
    local fix = fixture(kind)
    local _, code, err = testenv.verb(fix.work, { "regress" })
    t.eq(code, 0, err)
    t.ok(err:find("baseline: " .. fix.base .. "\n", 1, true), err)
end)

case("regress prints the full baseline sha for an explicit REV", function(kind)
    local fix = fixture(kind)
    local _, code, err = testenv.verb(fix.work, { "regress", "gerrit/main" })
    t.eq(code, 0, err)
    t.ok(err:find("baseline: " .. fix.moved .. "\n", 1, true), err)
end)

case("regress uses an explicit REV unchanged", function(kind)
    local fix = fixture(kind)
    local _, code, err = testenv.verb(fix.work, { "regress", "gerrit/main" })
    t.eq(code, 0, err)
    t.eq(seen(fix), "moved\n")
end)

case("regress leaves the working tree untouched", function(kind)
    local fix = fixture(kind)
    local before = git(fix.work, "status", "--porcelain")
    local _, code, err = testenv.verb(fix.work, { "regress" })
    t.eq(code, 0, err)
    t.eq((fs.read(fix.dir .. "/status_during"):gsub("%s+$", "")), before)
    t.eq(git(fix.work, "status", "--porcelain"), before)
    t.ok(not fs.exists(fix.work .. "/.ci-baseline"))
end)

case("regress ships the baseline inside the staged tree", function(kind)
    local fix = fixture(kind)
    local _, code, err = testenv.verb(fix.work, { "regress" })
    t.eq(code, 0, err)
    t.ok(fs.read(fix.dir .. "/where") ~= fix.work)
    t.eq(seen(fix), "base\n")
    t.eq(fs.read(fix.dir .. "/staged_marker"), "card\n")
    t.eq(fs.read(fix.dir .. "/staged_extra"), "extra\n")
    t.ok(not fs.exists(fix.dir .. "/staged_ignored"))
end)

case("regress removes the staged tree after a passing run", function(kind)
    local fix = fixture(kind)
    local _, code, err = testenv.verb(fix.work, { "regress" })
    t.eq(code, 0, err)
    t.ok(not fs.exists((fs.read(fix.dir .. "/where"):gsub("%s+$", ""))))
end)

case("regress removes the staged tree after a failing run", function(kind)
    local fix = fixture(kind)
    assert(fs.write(fix.dir .. "/fail", ""))
    local _, code = testenv.verb(fix.work, { "regress" })
    t.ok(code ~= 0)
    t.ok(not fs.exists((fs.read(fix.dir .. "/where"):gsub("%s+$", ""))))
end)

local function git_calls(fix)
    return table.concat(testenv.calls(fix.work, "git"), "\n")
end

t.test("regress reads the checkout through jj when .jj exists", function()
    local fix = fixture(Kind.JJ)
    local _, code, err = testenv.verb(fix.work, { "regress" })
    t.eq(code, 0, err)
    t.eq(seen(fix), "base\n")
    local jj = table.concat(testenv.calls(fix.work, "jj"), "\n")
    t.ok(jj:find("file list", 1, true), jj)
    t.ok(jj:find("heads(::@- & ::main@gerrit)", 1, true), jj)
    local git = git_calls(fix)
    t.ok(not git:find("ls-files", 1, true), git)
    t.ok(not git:find("merge-base", 1, true), git)
    t.ok(not git:find("rev-parse HEAD", 1, true), git)
end)

t.test("regress resolves an explicit REV through jj when .jj exists", function()
    local fix = fixture(Kind.JJ)
    local _, code, err = testenv.verb(fix.work, { "regress", "gerrit/main" })
    t.eq(code, 0, err)
    t.eq(seen(fix), "moved\n")
    local jj = table.concat(testenv.calls(fix.work, "jj"), "\n")
    t.ok(jj:find("-r main@gerrit", 1, true), jj)
    t.ok(not git_calls(fix):find("--verify", 1, true))
end)

t.test("regress reads the checkout through git when .jj is absent", function()
    local fix = fixture(Kind.GIT)
    local _, code, err = testenv.verb(fix.work, { "regress" })
    t.eq(code, 0, err)
    t.ok(not fs.exists(fix.work .. "/bin/jj.log"))
end)
