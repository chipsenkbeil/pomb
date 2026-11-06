--[[
Format the workspace with nightly rustfmt, which the unstable rustfmt.toml options need,
then drop the trailing period from each // one-liner.
]]
local devos = require("dev.os")
local lintset = require("lintset")
local verb = require("dev.verb")

local ctx = ...
verb.no_args(ctx)
local root = ctx.root

local code = devos.run({ "cargo", "+nightly", "fmt" }, { cwd = root })
if code ~= 0 then
    return code or 1
end

for _, file in ipairs(lintset.new(ctx):files({ "*.rs" })) do
    local ok, err = lintset.fix_comment_periods(root, file)
    if not ok then
        verb.die(tostring(err))
    end
end
