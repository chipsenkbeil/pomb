local fs = require("@dev/fs")
local site = require("@lib/pomb/site")
local t = require("@dev/testing")
local testkit = require("@lib/pomb/testkit")

for _, name in ipairs({ "decoys", "firewall", "restage", "setupcode", "status" }) do
    t.test("site.read_script returns the " .. name .. " script text", function()
        local text = site.read_script({ root = testkit.root }, name)
        t.eq(text:sub(1, 2), "#!")
    end)
end

t.test("the site, provision, verify and start-server messages carry no program prefix", function()
    local files = {
        "firewall.luau",
        "provision.luau",
        "verify.luau",
    }
    local banned = { '"site: ', '"provision: ', '"verify: ', "server did not start" }
    local paths = {}
    for _, file in ipairs(files) do
        table.insert(paths, ".dev/commands/site/" .. file)
    end
    table.insert(paths, ".dev/commands/parity/startserver.luau")
    for _, path in ipairs(paths) do
        local text = assert(fs.read(testkit.root .. "/" .. path))
        for _, prefix in ipairs(banned) do
            t.ok(text:find(prefix, 1, true) == nil, path .. " holds " .. prefix)
        end
    end
end)
