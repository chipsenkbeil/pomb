local devos = require("dev.os")
local fs = require("dev.fs")

---@class pomb.drain
local M = {}

local DRAIN_TEXT = "the hub is draining"
local INTERVAL_SECS = 60
local BOUND_SECS = 3600

local function positive_env(name, default)
    local value = tonumber(os.getenv(name))
    if value and value > 0 then
        return value
    end
    return default
end

local function drained(log)
    return (fs.read(log) or ""):find(DRAIN_TEXT, 1, true) ~= nil
end

---@class pomb.drain.Opts
---@field gate string gate name for the wait message
---@field log string path of the gate log
---@field run fun(): string?, integer? runs the gate and returns its output and exit code
---@field say fun(text: string) prints a progress line

---Run a gate, and run it again after each wait while it fails with a drain.
---POMB_DRAIN_INTERVAL sets the seconds between runs.
---POMB_DRAIN_BOUND sets the seconds a drain may last.
---@param opts pomb.drain.Opts
---@return string? ran output of the last run, nil when the drain outlasted the bound
---@return integer code exit code of the last run, or the bound in seconds when ran is nil
function M.run(opts)
    local interval = positive_env("POMB_DRAIN_INTERVAL", INTERVAL_SECS)
    local bound = positive_env("POMB_DRAIN_BOUND", BOUND_SECS)
    local ran, code = opts.run()
    local waited = 0
    while code ~= 0 and drained(opts.log) do
        if waited >= bound then
            return nil, bound
        end
        opts.say(
            ("The dev %s --remote run waits %s seconds because the ci hub is draining.\n"):format(
                opts.gate,
                interval
            )
        )
        devos.run({ "sleep", tostring(interval) })
        waited = waited + interval
        ran, code = opts.run()
    end
    return ran, code
end

return M
