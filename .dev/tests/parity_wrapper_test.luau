local fs = require("@dev/fs")
local t = require("@dev/testing")
local testenv = require("@dev/testenv")
local testkit = require("@lib/pomb/testkit")

local WRAPPERS = { "finder", "overflow", "regress", "run", "suggest" }

local WORDS = {
    finder = { "--candidate", "alpha" },
    overflow = { "alpha", "beta" },
    regress = { "--candidate", "alpha", "--baseline", "beta", "--out", "gamma" },
    run = { "--state", "alpha" },
    suggest = { "--state", "alpha", "--entry", "beta" },
}

local function wrapper(name, code)
    local root = assert(fs.realpath(testenv.tempdir()))
    assert(fs.symlink(testkit.root .. "/.dev", root .. "/.dev"))
    assert(fs.symlink(testkit.root .. "/.luaurc", root .. "/.luaurc"))
    testenv.fake(root, "node", { code = code })
    local _, status, err = testenv.command(root, { "parity", name, table.unpack(WORDS[name]) })
    return root, status, err
end

t.test("parity wrappers call node with their script", function()
    for _, name in ipairs(WRAPPERS) do
        local root, _, err = wrapper(name, 0)
        local calls = testenv.calls(root, "node")
        t.eq(#calls, 1, name .. ": " .. err)
        local want = root .. "/.dev/parity/" .. name .. ".mjs " .. table.concat(WORDS[name], " ")
        t.eq(calls[1], want, name)
    end
end)

t.test("parity wrappers return the node exit code", function()
    for _, name in ipairs(WRAPPERS) do
        local _, failed, failed_err = wrapper(name, 4)
        t.eq(failed, 4, name .. ": " .. failed_err)
        local _, passed, passed_err = wrapper(name, 0)
        t.eq(passed, 0, name .. ": " .. passed_err)
    end
end)
