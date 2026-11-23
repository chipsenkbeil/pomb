local fs = require("@dev/fs")
local t = require("@dev/testing")
local testenv = require("@dev/testenv")
local testkit = require("@lib/pomb/testkit")

local function build(words)
    local root = assert(fs.realpath(testenv.tempdir()))
    assert(fs.mkdir(root .. "/bin", { parents = true }))
    assert(fs.symlink(testkit.root .. "/.dev", root .. "/.dev"))
    assert(fs.symlink(testkit.root .. "/.luaurc", root .. "/.luaurc"))
    testkit.stub(root .. "/bin/cargo", 'echo "$@" >\'' .. root .. "/args'")
    local out, code, err = testenv.command(root, words, {
        env = { CI = "true", DEV_REMOTE = "0" },
    })
    return root, out, code, err
end

t.test("dev build runs cargo build --release --locked", function()
    local root, _, code, err = build({ "build" })
    t.eq(code, 0, err)
    t.eq(fs.read(root .. "/args"), "build --release --locked\n")
end)

t.test("dev build passes the extra words, flags included, to cargo", function()
    local root, _, code, err = build({ "build", "--manifest-path", "X" })
    t.eq(code, 0, err)
    t.eq(fs.read(root .. "/args"), "build --release --locked --manifest-path X\n")
end)
