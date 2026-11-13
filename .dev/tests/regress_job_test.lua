local fs = require("dev.fs")
local t = require("dev.testing")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

local JOBS = assert(fs.read(ROOT .. "/.ci/jobs.lua"))
local SERVER = assert(fs.read(ROOT .. "/.dev/parity/start-server.lua"))
local GLOB = '"$out/$src"/seed-*.err'
local LOGS = { "seed-gm.err", "seed-vt.err", "seed-ep.err", "seed-pn.err" }

t.test("the regress job names no seeder log", function()
    for _, log in ipairs(LOGS) do
        t.ok(not JOBS:find(log, 1, true), "jobs.lua still names " .. log)
    end
end)

t.test("the regress job copies seed-*.err by glob", function()
    t.ok(JOBS:find(GLOB, 1, true), "jobs.lua has no " .. GLOB .. " loop")
end)

t.test("every seeder log in start-server.lua matches the glob", function()
    local found = 0
    for _ in SERVER:gmatch("seed%-%a+%.err") do
        found = found + 1
    end
    t.eq(found, #LOGS)
end)
