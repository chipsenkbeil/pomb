local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

local function project(fakes)
    local root = assert(fs.realpath(testenv.tempdir()))
    assert(fs.mkdir(root .. "/.dev/tests", { parents = true }))
    assert(fs.symlink(ROOT .. "/.dev/test.lua", root .. "/.dev/test.lua"))
    assert(fs.symlink(ROOT .. "/.dev/lua", root .. "/.dev/lua"))
    assert(fs.mkdir(root .. "/tests/js", { parents = true }))
    assert(fs.write(root .. "/tests/js/alpha.js", ""))
    assert(fs.write(root .. "/tests/js/beta.js", ""))
    testenv.fake(root, "cargo", { code = fakes.cargo or 0 })
    testenv.fake(root, "node", { code = fakes.node or 0 })
    return root
end

t.test("test_verb_should_run_every_step_without_arguments", function()
    local root = project({})
    local _, code, err = testenv.verb(root, { "test" })
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

t.test("test_verb_should_skip_the_doc_tests_with_arguments", function()
    local root = project({})
    local _, code, err = testenv.verb(root, { "test", "--", "-j", "1", "module" })
    t.eq(code, 0, err)
    t.eq(testenv.calls(root, "cargo"), { "nextest run --all-features -j 1 module" })
    t.eq(#testenv.calls(root, "node"), 2)
end)

t.test("test_verb_should_stop_at_the_first_failing_step", function()
    local root = project({ cargo = 3 })
    local _, code, err = testenv.verb(root, { "test" })
    t.eq(code, 3, err)
    t.eq(testenv.calls(root, "cargo"), { "nextest run --all-features" })
    t.eq(testenv.calls(root, "node"), {})

    root = project({ node = 5 })
    _, code, err = testenv.verb(root, { "test" })
    t.eq(code, 5, err)
    t.eq(testenv.calls(root, "node"), { "tests/js/alpha.js static/app.js" })
end)
