local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

local function project(fakes)
    local root = assert(fs.realpath(testenv.tempdir()))
    assert(fs.mkdir(root .. "/bin", { parents = true }))
    assert(fs.symlink(ROOT .. "/.dev", root .. "/.dev"))
    testenv.fake(root, "cargo", { code = fakes.cargo or 0 })
    testenv.fake(root, "stylua", { code = 0 })
    return root
end

t.test("fmt refuses a stray flag", function()
    local root = project({})
    local _, code, err = testenv.verb(root, { "fmt", "--bogus" })
    t.eq(code, 2, err)
    t.ok(err:find("Unknown flag --bogus.", 1, true), err)
    t.eq(testenv.calls(root, "cargo"), {})
end)

t.test("fmt passes the cargo exit code through", function()
    local root = project({ cargo = 3 })
    local _, code, err = testenv.verb(root, { "fmt" })
    t.eq(code, 3, err)
    t.eq(testenv.calls(root, "cargo"), { "+nightly fmt" })
    t.eq(testenv.calls(root, "stylua"), {})
end)

t.test("fmt runs nightly rustfmt", function()
    local root = project({})
    local _, code, err = testenv.verb(root, { "fmt" })
    t.eq(code, 0, err)
    t.eq(testenv.calls(root, "cargo"), { "+nightly fmt" })
end)
