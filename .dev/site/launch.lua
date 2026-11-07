--[[
local
positional
Enable and start a channel's service and update timer: dev site launch <stable|nightly>
]]
local site = require("pomb.site")

local ctx = ...
local channel = ctx.args[1] or ""
if not site.is_channel(channel) then
    io.stderr:write("usage: dev site launch <stable|nightly>\n")
    return site.USAGE
end

local service = "pomb-" .. channel .. ".service"
local timer = "pomb-update@" .. channel .. ".timer"
local code = site.ssh({ "systemctl", "enable", "--now", service, timer })
if code ~= 0 then
    return code
end
return site.ssh({ "systemctl", "is-active", service, timer })
