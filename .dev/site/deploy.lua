--[[
local
positional
Run the box's pull agent now: dev site deploy <stable|nightly> [<tag>]
]]
local site = require("pomb.site")

local ctx = ...
local channel = ctx.args[1] or ""
local tag = ctx.args[2] or ""
if not site.is_channel(channel) then
    io.stderr:write("usage: dev site deploy <stable|nightly> [<tag>]\n")
    return site.USAGE
end

local words = { "/usr/local/sbin/pomb-update", channel }
if tag ~= "" then
    words[#words + 1] = tag
end
return site.ssh(words)
