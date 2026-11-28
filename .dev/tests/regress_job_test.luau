local fs = require("@dev/fs")
local t = require("@dev/testing")
local testkit = require("@lib/pomb/testkit")

local ROOT = testkit.root

local JOBS = assert(fs.read(ROOT .. "/.ci/jobs.lua"))
local SERVER = assert(fs.read(ROOT .. "/.dev/commands/parity/startserver.luau"))
local GLOB = '"$out/$src"/seed-*.err'
local LOGS: { string } = {}
local seen: { [string]: boolean } = {}
for name in SERVER:gmatch("seed%-%w+%.err") do
    if not seen[name] then
        seen[name] = true
        LOGS[#LOGS + 1] = name
    end
end

t.test("the regress job names no seeder log", function()
    for _, log in ipairs(LOGS) do
        t.ok(not JOBS:find(log, 1, true), "jobs.lua still names " .. log)
    end
end)

t.test("the regress job copies seed-*.err by glob", function()
    t.ok(JOBS:find(GLOB, 1, true), "jobs.lua has no " .. GLOB .. " loop")
end)

t.test("seeder logs are read from start-server", function()
    t.ok(#LOGS > 0, "startserver.luau names no seeder log")
    for _, log in ipairs(LOGS) do
        t.ok(log:find("^seed%-.+%.err$") ~= nil, `{log} does not match seed-*.err`)
    end
end)
