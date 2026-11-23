local site = require("@lib/pomb/site")
local t = require("@dev/testing")
local testkit = require("@lib/pomb/testkit")

for _, name in ipairs({ "decoys", "firewall", "restage", "setupcode", "status" }) do
    t.test("site.read_script returns the " .. name .. " script text", function()
        local text = site.read_script({ root = testkit.root }, name)
        t.eq(text:sub(1, 2), "#!")
    end)
end
