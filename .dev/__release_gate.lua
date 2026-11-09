--[[
hidden
positional
Run one release gate with its own drain retry.
Usage: dev __release_gate <worktree> <gate> <logs>. Writes <logs>/<gate>.log, <logs>/<gate>.err and <logs>/<gate>.ran. Exits 0 when the gate passed, 1 when it failed, 3 when a drain outlasted the bound, and then .ran holds the bound in seconds.
]]
local devos = require("dev.os")
local drain = require("pomb.drain")
local fs = require("dev.fs")

local ctx = ...

local wt, gate, logs = ctx.args[1], ctx.args[2], ctx.args[3]
if not (wt and gate and logs) or wt == "" or gate == "" or logs == "" then
    io.stderr:write("usage: __release_gate <worktree> <gate> <logs>\n")
    return 2
end

local log = logs .. "/" .. gate .. ".log"
local ran, code = drain.run({
    gate = gate,
    log = log,
    say = function(text)
        io.stdout:write(text)
        io.stdout:flush()
    end,
    run = function()
        return devos.capture({ "dev", "__gate", wt, gate, log }, {
            cwd = wt,
            env = { CARGO_TARGET_DIR = wt .. "/target" },
            stderr = logs .. "/" .. gate .. ".err",
        })
    end,
})
if not ran then
    assert(fs.write(logs .. "/" .. gate .. ".ran", tostring(code)))
    return 3
end
assert(fs.write(logs .. "/" .. gate .. ".ran", ran))
if code ~= 0 then
    return 1
end
return 0
