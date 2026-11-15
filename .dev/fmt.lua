--[[
Format the workspace with nightly rustfmt, which the unstable rustfmt.toml options need,
then drop the trailing period from each // one-liner, then format the .dev Lua files.
]]
local dev_os = require("dev.os")
local lintset = require("lintset")
local verb = require("dev.verb")

local ctx = ...
verb.no_args(ctx)
local root = ctx.root

local code = dev_os.run({ "cargo", "+nightly", "fmt" }, { cwd = root })
if code ~= 0 then
    return code or 1
end

for _, file in ipairs(lintset.new(ctx):files({ "*.rs" })) do
    local ok, err = lintset.fix_comment_periods(root, file)
    if not ok then
        verb.die(tostring(err))
    end
end

local lua_files = {}
for _, file in ipairs(lintset.new(ctx):files({ "*.lua" })) do
    if file:find("^%.dev/") and not file:find("^%.dev/worktrees/") then
        lua_files[#lua_files + 1] = file
    end
end
if #lua_files == 0 then
    return 0
end
if not dev_os.which("stylua") then
    verb.die("Stylua is not installed. Run `brew install stylua`.")
end
return dev_os.run(lintset.with_files({ "stylua" }, lua_files), { cwd = root })
