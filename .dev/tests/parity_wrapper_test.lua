local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

local WRAPPERS = { "finder", "overflow", "regress", "run", "suggest" }

local function wrapper(name, code)
    local root = assert(fs.realpath(testenv.tempdir()))
    assert(fs.symlink(ROOT .. "/.dev", root .. "/.dev"))
    testenv.fake(root, "node", { code = code })
    local _, status, err = testenv.verb(root, { "parity", name, "alpha", "beta" })
    return root, status, err
end

t.test("parity_wrappers_should_call_node_with_their_script", function()
    for _, name in ipairs(WRAPPERS) do
        local root, _, err = wrapper(name, 0)
        local calls = testenv.calls(root, "node")
        t.eq(#calls, 1, name .. ": " .. err)
        t.eq(calls[1], root .. "/.dev/parity/" .. name .. ".mjs alpha beta", name)
    end
end)

t.test("parity_wrappers_should_return_the_node_exit_code", function()
    for _, name in ipairs(WRAPPERS) do
        local _, failed, failed_err = wrapper(name, 4)
        t.eq(failed, 4, name .. ": " .. failed_err)
        local _, passed, passed_err = wrapper(name, 0)
        t.eq(passed, 0, name .. ": " .. passed_err)
    end
end)
