local fs = require("@dev/fs")
local t = require("@dev/testing")
local testenv = require("@dev/testenv")
local testkit = require("@lib/pomb/testkit")

local LOCAL = { env = { DEV_REMOTE = "0" } }

local function project(fakes)
    local root = assert(fs.realpath(testenv.tempdir()))
    assert(fs.mkdir(root .. "/.dev/tests", { parents = true }))
    assert(fs.symlink(testkit.root .. "/.dev/commands", root .. "/.dev/commands"))
    assert(fs.symlink(testkit.root .. "/.dev/lib", root .. "/.dev/lib"))
    assert(fs.symlink(testkit.root .. "/.luaurc", root .. "/.luaurc"))
    assert(fs.mkdir(root .. "/tests/js", { parents = true }))
    assert(fs.write(root .. "/tests/js/alpha.js", ""))
    assert(fs.write(root .. "/tests/js/beta.js", ""))
    testenv.fake(root, "cargo", { code = fakes.cargo or 0 })
    testenv.fake(root, "node", { code = fakes.node or 0 })
    return root
end

t.test("the test verb runs every step without arguments", function()
    local root = project({})
    local _, code, err = testenv.command(root, { "test" }, LOCAL)
    t.eq(code, 0, err)
    t.eq(testenv.calls(root, "cargo"), {
        "nextest run --all-features",
        "test --doc --all-features",
    })
    t.eq(testenv.calls(root, "node"), {
        "tests/js/alpha.js static/app.js",
        "tests/js/beta.js static/app.js",
    })
end)

t.test("the test verb skips the doc tests with arguments", function()
    local root = project({})
    local _, code, err = testenv.command(root, { "test", "--", "-j", "1", "module" }, LOCAL)
    t.eq(code, 0, err)
    t.eq(testenv.calls(root, "cargo"), { "nextest run --all-features -j 1 module" })
    t.eq(#testenv.calls(root, "node"), 2)
end)

t.test("the test verb passes flags after -- on to nextest", function()
    local root = project({})
    local _, code, err = testenv.command(root, { "test", "--", "-j", "1", "--no-fail-fast" }, LOCAL)
    t.eq(code, 0, err)
    t.eq(testenv.calls(root, "cargo"), { "nextest run --all-features -j 1 --no-fail-fast" })
end)

t.test("the test verb stops at the first failing step", function()
    local root = project({ cargo = 3 })
    local _, code, err = testenv.command(root, { "test" }, LOCAL)
    t.eq(code, 3, err)
    t.eq(testenv.calls(root, "cargo"), { "nextest run --all-features" })
    t.eq(testenv.calls(root, "node"), {})

    root = project({ node = 5 })
    _, code, err = testenv.command(root, { "test" }, LOCAL)
    t.eq(code, 5, err)
    t.eq(testenv.calls(root, "node"), { "tests/js/alpha.js static/app.js" })
end)
