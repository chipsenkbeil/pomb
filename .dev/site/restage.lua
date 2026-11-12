--[[
local
positional
flags: --bad
Make the next update real: dev site restage nightly [--bad]
]]
local site = require("pomb.site")

local BAD_FLAG = "--bad"

local ctx = ...
local channel = ctx.args[1] or ""
local mode = ctx.args[2] or ""
if channel ~= site.CHANNEL.NIGHTLY or (mode ~= "" and mode ~= BAD_FLAG) then
    io.stderr:write("usage: dev site restage nightly [--bad]\n")
    return site.USAGE
end

local script = site.read_script(ctx, "restage")
return site.ssh_script(script, { "BAD=" .. (mode ~= "" and "1" or ""), "bash", "-s" })
