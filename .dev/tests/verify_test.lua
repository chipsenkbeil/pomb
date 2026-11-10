local backup = require("pomb.backup")
local t = require("dev.testing")

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
