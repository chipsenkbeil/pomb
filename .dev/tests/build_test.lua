local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

t.test("dev build refuses an unknown flag with exit 2 and its message", function()
    local out, code, err = testenv.verb(ROOT, { "build", "--bogus" }, {
        env = { CI = "true", DEV_REMOTE = "0" },
    })
    t.eq(code, 2)
    t.ok(err:find("dev build: unknown flag --bogus", 1, true), err)
end)
