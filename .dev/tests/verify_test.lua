local backup = require("pomb.backup")
local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

local function verify_with_fake_ssh(args)
    local dir = assert(fs.realpath(testenv.tempdir()))
    assert(fs.mkdir(dir .. "/bin", { parents = true }))
    assert(fs.write(dir .. "/bin/ssh", "#!/bin/sh\necho ran >>'" .. dir .. "/ssh.log'\n"))
    assert(devos.run({ "chmod", "755", dir .. "/bin/ssh" }) == 0)
    local _, code, err = testenv.verb(ROOT, args, {
        env = { PATH = dir .. "/bin:" .. os.getenv("PATH") },
    })
    local ssh = fs.read(dir .. "/ssh.log")
    fs.remove(dir, { recursive = true })
    return code, err, ssh
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
