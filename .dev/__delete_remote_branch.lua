--[[
hidden
positional
Delete branches on the remote of record that the owner named as done.
Usage: dev __delete_remote_branch <branch>... Prints each branch tip before deleting it, so a mistaken delete can be restored by pushing that sha back to the branch.
]]
local devos = require("dev.os")
local forge = require("pomb.forge")

local ctx = ...

if #ctx.args == 0 then
    io.stderr:write("usage: __delete_remote_branch.sh <branch>...\n")
    return 2
end

local remote, err = forge.remote(ctx.root)
if not remote then
    io.stderr:write(err, "\n")
    return 1
end

local code = devos.run({ "git", "fetch", "-q", remote }, { cwd = ctx.root })
if code ~= 0 then
    return code or 1
end
for _, branch in ipairs(ctx.args) do
    if branch == "main" or branch == "HEAD" or branch == "" then
        io.stderr:write("refuse  ", branch, "\n")
        return 1
    end
    local tip
    tip, code = devos.capture(
        { "git", "rev-parse", "-q", "--verify", "refs/remotes/" .. remote .. "/" .. branch },
        { cwd = ctx.root, stderr = "null" }
    )
    if code ~= 0 then
        io.stderr:write("missing ", remote, "/", branch, "\n")
        return 1
    end
    io.stdout:write("delete  ", remote, "/", branch, " at ", (tip:gsub("%s+$", "")), "\n")
    io.stdout:flush()
    code = devos.run(
        { "jj-git-guard", "push", "--", remote, ":refs/heads/" .. branch },
        { cwd = ctx.root }
    )
    if code ~= 0 then
        return code or 1
    end
end
return 0
