local devos = require("@dev/os")
local dev_command = require("@dev/command")

local HOST = "green-box"
local UNITS = { pomb = true, caddy = true }
local SECONDS_PER_MINUTE = 60
local MINUTES_PER_HOUR = 60
local HOURS_PER_DAY = 24
local MONTHS_PER_YEAR = 12
local MAX_DAY = 31

local function utc_time(text)
    local stamp = text:match("^(%d%d%d%d%-%d%d%-%d%d %d%d:%d%d:%d%d)$")
        or text:match("^(%d%d%d%d%-%d%d%-%d%d %d%d:%d%d:%d%d) UTC$")
        or text:match("^(%d%d%d%d%-%d%d%-%d%d %d%d:%d%d:%d%d)Z$")
    if stamp == nil then
        return nil
    end
    local month, day, hour, minute, second = stamp:match("^%d+%-(%d+)%-(%d+) (%d+):(%d+):(%d+)$")
    local ok = tonumber(month) >= 1
        and tonumber(month) <= MONTHS_PER_YEAR
        and tonumber(day) >= 1
        and tonumber(day) <= MAX_DAY
        and tonumber(hour) < HOURS_PER_DAY
        and tonumber(minute) < MINUTES_PER_HOUR
        and tonumber(second) < SECONDS_PER_MINUTE
    return ok and stamp or nil
end

return dev_command({
    summary = "Print the green-box journal for pomb or caddy between two UTC times.",
    remote = false,
    flags = {
        {
            name = "since",
            kind = "string",
            help = "the first UTC time, as 2026-11-11 16:30:00",
        },
        {
            name = "until",
            kind = "string",
            help = "the last UTC time, as 2026-11-11 17:30:00",
        },
        {
            name = "unit",
            kind = "string",
            help = "pomb (the default) or caddy, the proxy",
        },
    },
    long = [[
Read-only: runs one ssh to green-box with journalctl for the unit between the two times, and prints the lines. Both times are UTC, written as YYYY-MM-DD HH:MM:SS, and --since must come before --until. A missing, unordered or non-UTC time exits 2 with a message.
]],
    run = function(opts, _, ctx)
        -- Check the unit and both times
        local unit = opts.unit or "pomb"
        if not UNITS[unit] then
            dev_command.usage_error(ctx, "The --unit option takes pomb or caddy.")
        end
        if opts.since == nil or opts["until"] == nil then
            dev_command.usage_error(ctx, "Both --since and --until are needed.")
        end
        local since = utc_time(opts.since)
        local stop = utc_time(opts["until"])
        if since == nil or stop == nil then
            dev_command.usage_error(ctx, "Each time must be UTC, as YYYY-MM-DD HH:MM:SS.")
        end
        if since >= stop then
            dev_command.usage_error(ctx, "The --since time must be before the --until time.")
        end

        -- Read the journal over one ssh
        local command = table.concat({
            "journalctl",
            "-u",
            unit,
            "--utc",
            "--no-pager",
            "--since",
            devos.quote(since .. " UTC"),
            "--until",
            devos.quote(stop .. " UTC"),
        }, " ")
        return devos.run({ "ssh", HOST, command })
    end,
})
