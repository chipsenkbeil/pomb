local backup = require("pomb.backup")
local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")
local gitenv = require("pomb.gitenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

devos.defaults.env = gitenv.env

local git = gitenv.git

local function release_fixture()
    local dir = assert(fs.realpath(testenv.tempdir()))
    local work = dir .. "/work"
    git(dir, "init", "-q", "--bare", "remote.git")
    git(dir, "clone", "-q", "remote.git", "work")
    git(work, "remote", "rename", "origin", "gerrit")
    git(work, "checkout", "-q", "-b", "main")
    assert(fs.symlink(ROOT .. "/.dev", work .. "/.dev"))
    assert(fs.mkdir(work .. "/static"))
    assert(fs.write(work .. "/static/app.js", "app\n"))
    git(work, "add", "static/app.js")
    git(work, "commit", "-q", "-m", "release")
    git(work, "push", "-q", "gerrit", "main")
    return dir, work
end

local function write_stubs(dir)
    local bin = dir .. "/bin"
    assert(fs.mkdir(bin, { parents = true }))
    assert(fs.write(bin .. "/ssh", "#!/bin/sh\necho ran >>'" .. dir .. "/ssh.log'\n"))
    assert(fs.write(
        bin .. "/curl",
        "#!/bin/sh\necho \"$@\" >>'" .. dir .. "/curl.log'\nprintf 'HTTP/2 200\\r\\n\\r\\n'\n"
    ))
    assert(devos.run({ "chmod", "755", bin .. "/ssh", bin .. "/curl" }) == 0)
    return bin
end

local function verify_with_fake_ssh(args)
    local dir = assert(fs.realpath(testenv.tempdir()))
    local bin = write_stubs(dir)
    local _, code, err = testenv.verb(ROOT, args, {
        env = { PATH = bin .. ":" .. os.getenv("PATH") },
    })
    local ssh = fs.read(dir .. "/ssh.log")
    fs.remove(dir, { recursive = true })
    return code, err, ssh
end

local function verify_on_release_fixture()
    local dir, work = release_fixture()
    local bin = write_stubs(dir)
    local both = dir .. "/both.txt"
    local code = devos.run({ "dev", "verify", "HEAD" }, {
        cwd = work,
        env = { PATH = bin .. ":" .. os.getenv("PATH") },
        stdout = both,
        stderr = "stdout",
    })
    local text = assert(fs.read(both))
    local curl = fs.read(dir .. "/curl.log")
    fs.remove(dir, { recursive = true })
    return code, text, curl
end

t.test("backup resolves a bare file name under the backup directory", function()
    local path, why = backup.resolve("pomb-1.redb")
    t.eq(path, "/var/lib/pomb-backups/pomb-1.redb")
    t.eq(why, nil)
end)

t.test("backup resolves a full path under the backup directory", function()
    local path, why = backup.resolve("/var/lib/pomb-backups/pomb-1.redb")
    t.eq(path, "/var/lib/pomb-backups/pomb-1.redb")
    t.eq(why, nil)
end)

t.test("backup refuses an absolute path outside the backup directory", function()
    local path, why = backup.resolve("/etc/passwd")
    t.eq(path, nil)
    local expected = "The --backup argument must be a file name or a path under "
        .. "/var/lib/pomb-backups, not /etc/passwd."
    t.eq(why, expected)
end)

t.test("backup refuses a path that climbs out of the backup directory", function()
    local path = backup.resolve("/var/lib/pomb-backups/../secrets")
    t.eq(path, nil)
    t.eq(backup.resolve("../secrets"), nil)
end)

t.test("backup refuses an empty argument and the bare directory", function()
    t.eq(backup.resolve(""), nil)
    t.eq(backup.resolve("/var/lib/pomb-backups/"), nil)
end)

t.test("bare_backup_usage refuses a --backup with no value before any ssh check", function()
    local cases = {
        { "verify", "HEAD", "--backup" },
        { "verify", "HEAD", "--backup", "" },
        { "verify", "HEAD", "--backup", "--cleanup", ".dev/worktrees/x" },
    }
    for _, args in ipairs(cases) do
        local code, err, ssh = verify_with_fake_ssh(args)
        t.eq(code, 2)
        t.ok(err:find("usage: dev verify <release-sha>", 1, true), err)
        t.eq(ssh, nil)
    end
end)

t.test("verify_login_probe_should_use_the_curl_on_path", function()
    local code, text, curl = verify_on_release_fixture()
    t.eq(code, 1)
    t.ok(curl ~= nil and curl:find("https://pomb.chip.network/login", 1, true) ~= nil, text)
end)

t.test("verify_summary_should_follow_every_check_line_in_one_file", function()
    local code, text = verify_on_release_fixture()
    local lines = {}
    for line in text:gmatch("[^\n]+") do
        lines[#lines + 1] = line
    end
    t.eq(code, 1)
    local expected = "check(s) failed, so the worktrees are untouched."
    t.ok(lines[#lines]:find(expected, 1, true) ~= nil, text)
    local checks = 0
    for i, line in ipairs(lines) do
        if line:match("^PASS ") or line:match("^FAIL ") then
            checks = checks + 1
            t.ok(i < #lines, text)
        end
    end
    t.ok(checks > 0, text)
end)
