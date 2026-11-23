--[[
Format the Rust workspace with nightly rustfmt and the Lua under .dev and .ci with stylua.
Nightly is needed for the unstable rustfmt.toml options, and each // one-liner then loses its
trailing period.
patch fmt fmt.patch
]]
local dev_os = require("dev.os")
local lintset = require("lintset")
local dev_command = require("dev.command")

local ctx = ...
dev_command.no_args(ctx)
local root = ctx.root

local code = dev_os.run({ "cargo", "+nightly", "fmt" }, { cwd = root })
if code ~= 0 then
    return code or 1
end

local run = lintset.new(ctx)
local dev_lua = run:dev_lua()
if #dev_lua > 0 then
    local argv = lintset.with_files({ "stylua", "--config-path", "stylua.toml" }, dev_lua)
    code = dev_os.run(argv, { cwd = root })
    if code ~= 0 then
        return code or 1
    end
end

for _, file in ipairs(run:files({ "*.rs" })) do
    local ok, err = lintset.fix_comment_periods(root, file)
    if not ok then
        dev_command.die(tostring(err))
    end
end
