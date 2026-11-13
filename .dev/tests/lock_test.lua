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

local git = gitenv.git

local STUB_CI = [[
case "$1" in
-C)
    cd "$2" || exit 2
    pwd -P >'@dir@/where'
    echo "$@" >'@dir@/args'
    git -C '@work@' status --porcelain >'@dir@/status_during'
    echo "run 7"
    if [ -e '@dir@/fail' ]; then exit 1; fi
    exit 0
    ;;
ps)
    echo "  Cargo.lock  1 KiB"
    exit 0
    ;;
cp)
    echo "$2" >'@dir@/copied'
    printf 'fetched\n' >"$3/Cargo.lock"
    exit 0
    ;;
esac
exit 3
]]

local function fixture(kind)
    local dir = assert(fs.realpath(testenv.tempdir()))
    local work = dir .. "/work"
    assert(fs.mkdir(work))
    git(work, "init", "-q")
    assert(fs.symlink(ROOT .. "/.dev", work .. "/.dev"))
    assert(fs.write(work .. "/Cargo.lock", "original\n"))
    assert(fs.write(work .. "/other", "other\n"))
    git(work, "add", "Cargo.lock", "other")
    git(work, "commit", "-q", "-m", "base")
    assert(fs.mkdir(work .. "/bin"))
    local stub = work .. "/bin/ci"
    local body = STUB_CI:gsub("@dir@", dir):gsub("@work@", work)
    assert(fs.write(stub, "#!/bin/sh\n" .. body))
    assert(devos.run({ "chmod", "755", stub }) == 0)
    if kind == Kind.JJ then
        jjenv.install(work)
    end
    return { dir = dir, work = work }
end

case("lock_should_refuse_a_missing_package", function(kind)
    local fix = fixture(kind)
    local _, code, err = testenv.verb(fix.work, { "lock" })
    t.ok(code ~= 0)
    t.ok(err:find("Name the package to resolve.", 1, true), err)
    t.ok(not fs.exists(fix.dir .. "/where"))
    t.eq(fs.read(fix.work .. "/Cargo.lock"), "original\n")
end)

case("lock_should_write_the_fetched_lock_file", function(kind)
    local fix = fixture(kind)
    local before = git(fix.work, "status", "--porcelain")
    local _, code, err = testenv.verb(fix.work, { "lock", "webauthn-rs", "--precise", "0.6.2" })
    t.eq(code, 0, err)
    t.eq(fs.read(fix.work .. "/Cargo.lock"), "fetched\n")
    t.eq(fs.read(fix.work .. "/other"), "other\n")
    local expected = " M Cargo.lock\n" .. before
    t.eq(git(fix.work, "status", "--porcelain"), expected)
    t.ok(err:find("run: 7\n", 1, true), err)
    t.ok(fs.read(fix.dir .. "/args"):find("webauthn-rs 0.6.2", 1, true))
end)

case("lock_should_remove_the_temp_dir_after_a_passing_run", function(kind)
    local fix = fixture(kind)
    local _, code, err = testenv.verb(fix.work, { "lock", "webauthn-rs" })
    t.eq(code, 0, err)
    t.ok(not fs.exists((fs.read(fix.dir .. "/where"):gsub("%s+$", ""))))
end)

case("lock_should_remove_the_temp_dir_after_a_failing_run", function(kind)
    local fix = fixture(kind)
    assert(fs.write(fix.dir .. "/fail", ""))
    local _, code = testenv.verb(fix.work, { "lock", "webauthn-rs" })
    t.ok(code ~= 0)
    t.ok(not fs.exists((fs.read(fix.dir .. "/where"):gsub("%s+$", ""))))
    t.eq(fs.read(fix.work .. "/Cargo.lock"), "original\n")
end)

t.test("lock stages the working copy through jj when .jj exists", function()
    local fix = fixture(Kind.JJ)
    local _, code, err = testenv.verb(fix.work, { "lock", "webauthn-rs" })
    t.eq(code, 0, err)
    local jj = table.concat(testenv.calls(fix.work, "jj"), "\n")
    t.ok(jj:find("file list", 1, true), jj)
    t.ok(not table.concat(testenv.calls(fix.work, "git"), "\n"):find("ls-files", 1, true))
end)

t.test("lock stages the working copy through git when .jj is absent", function()
    local fix = fixture(Kind.GIT)
    local _, code, err = testenv.verb(fix.work, { "lock", "webauthn-rs" })
    t.eq(code, 0, err)
    t.ok(not fs.exists(fix.work .. "/bin/jj.log"))
end)
