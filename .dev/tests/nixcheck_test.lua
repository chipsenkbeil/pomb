local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

local SANDBOX_FAILS = [[if [ "$(eval echo \${$#})" = true ]; then exit 0; fi
exit 5]]

local function stub(root, name, body)
    local path = root .. "/bin/" .. name
    assert(fs.write(path, "#!/bin/sh\n" .. body .. "\n"))
    assert(devos.run({ "chmod", "755", path }) == 0, "chmod failed")
end

local function nixcheck(target, unshare_body)
    local root = assert(fs.realpath(testenv.tempdir()))
    assert(fs.mkdir(root .. "/bin", { parents = true }))
    assert(fs.symlink(ROOT .. "/.dev", root .. "/.dev"))
    stub(root, "unshare", unshare_body)
    stub(root, "cargo", "exit 0")
    stub(root, "cargo-nextest", "exit 0")
    return testenv.verb(root, { "nixcheck" }, { env = { CARGO_TARGET_DIR = target } })
end

t.test("dev nixcheck refuses a target directory under /var/tmp", function()
    local _, code, err = nixcheck("/var/tmp/pomb-nixcheck-target", "exit 0")
    t.eq(code, 1)
    t.ok(err:find("under /var/tmp", 1, true), err)
    t.ok(err:find("CARGO_TARGET_DIR", 1, true), err)
end)

t.test("dev nixcheck refuses /var/tmp reached through dot dot", function()
    local _, code, err = nixcheck("/var/tmp/../tmp/pomb-nixcheck-target", "exit 0")
    t.eq(code, 1)
    t.ok(err:find("under /var/tmp", 1, true), err)
end)

t.test("dev nixcheck passes the sandbox exit code with the default target directory", function()
    local _, code, err = nixcheck("", SANDBOX_FAILS)
    t.eq(code, 5, err)
end)

t.test("dev nixcheck runs the sandbox for a target directory outside /var/tmp", function()
    local _, code, err = nixcheck("/tmp/pomb-nixcheck-target", SANDBOX_FAILS)
    t.eq(code, 5, err)
end)
