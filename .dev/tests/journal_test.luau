local devos = require("@dev/os")
local fs = require("@dev/fs")
local t = require("@dev/testing")
local testenv = require("@dev/testenv")
local testkit = require("@lib/pomb/testkit")

local ROOT = testkit.root

local function journal_with_fake_ssh(args)
    local dir = assert(fs.realpath(testenv.tempdir()))
    local bin = dir .. "/bin"
    assert(fs.mkdir(bin, { parents = true }))
    assert(fs.write(bin .. "/ssh", '#!/bin/sh\necho "$@" >>\'' .. dir .. "/ssh.log'\n"))
    assert(devos.run({ "chmod", "755", bin .. "/ssh" }) == 0)
    local words = { "journal" }
    for _, word in ipairs(args) do
        words[#words + 1] = word
    end
    local _, code, err = testenv.command(ROOT, words, {
        env = { PATH = bin .. ":" .. devos.getenv("PATH") },
    })
    local ssh = fs.read(dir .. "/ssh.log")
    fs.remove(dir, { recursive = true })
    return code, err, ssh
end

t.test("journal builds one journalctl call for the unit and window", function()
    -- Check the default unit and window
    local window = { "--since", "2026-11-11 16:30:00", "--until", "2026-11-11 17:30:00" }
    local code, _, ssh = journal_with_fake_ssh(window)
    t.eq(code, 0)
    local expected = "green-box journalctl -u pomb --utc --no-pager"
        .. " --since '2026-11-11 16:30:00 UTC' --until '2026-11-11 17:30:00 UTC'\n"
    t.eq(ssh, expected)

    -- Check the caddy unit with the Z and UTC suffixes
    local caddy = {
        "--unit",
        "caddy",
        "--since",
        "2026-11-11 16:30:00Z",
        "--until",
        "2026-11-11 17:30:00 UTC",
    }
    local caddy_code, _, caddy_ssh = journal_with_fake_ssh(caddy)
    t.eq(caddy_code, 0)
    t.eq(caddy_ssh, (expected:gsub("%-u pomb", "-u caddy")))
end)

t.test("journal refuses a missing or unordered window", function()
    local cases = {
        {},
        { "--since", "2026-11-11 16:30:00" },
        { "--until", "2026-11-11 17:30:00" },
        { "--since", "2026-11-11 17:30:00", "--until", "2026-11-11 16:30:00" },
        { "--since", "2026-11-11 16:30:00", "--until", "2026-11-11 16:30:00" },
        { "--since", "2026-11-11 16:30:00 +0100", "--until", "2026-11-11 17:30:00" },
        { "--since", "yesterday", "--until", "2026-11-11 17:30:00" },
        { "--since", "2026-13-11 16:30:00", "--until", "2026-11-12 17:30:00" },
        { "--unit", "nginx", "--since", "2026-11-11 16:30:00", "--until", "2026-11-11 17:30:00" },
    }
    for _, args in ipairs(cases) do
        local code, err, ssh = journal_with_fake_ssh(args)
        t.eq(code, 2, table.concat(args, " "))
        t.eq(ssh, nil)
        t.ok(err:find("journal", 1, true) ~= nil, err)
    end
end)
