local fs = require("dev.fs")
local site = require("pomb.site")
local t = require("dev.testing")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

for _, name in ipairs({ "decoys", "firewall", "restage", "setup-code", "status" }) do
    t.test("site.read_script returns the " .. name .. " script text", function()
        local text = site.read_script({ root = ROOT }, name)
        t.eq(text:sub(1, 2), "#!")
    end)
end
