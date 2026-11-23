---Delete branches on the remote of record that the owner named as done.
---Usage: dev -l delete_remote_branch.luau <branch>... Prints each branch tip before deleting it, so a mistaken delete can be restored by pushing that sha back to the branch.
local dev_command = require("@dev/command")
local dev_os = require("@dev/os")
local forge = require("@lib/pomb/forge")
local project = require("@dev/project")

local words = { ... }

if #words == 0 then
    dev_command.note("usage: delete_remote_branch.luau <branch>...")
    return 2
end

local root = project.root()
local remote, err = forge.remote(root)
if not remote then
    dev_command.note(err)
    return 1
end

local code = dev_os.run({ "git", "fetch", "-q", remote }, { cwd = root })
if code ~= 0 then
    return code or 1
end
for _, branch in ipairs(words) do
    if branch == "main" or branch == "HEAD" or branch == "" then
        dev_command.note("Refused to delete " .. branch .. ".")
        return 1
    end
    local tip
    tip, code = dev_os.capture(
        { "git", "rev-parse", "-q", "--verify", "refs/remotes/" .. remote .. "/" .. branch },
        { cwd = root, stderr = "null" }
    )
    if code ~= 0 then
        dev_command.note("No branch " .. branch .. " exists on " .. remote .. ".")
        return 1
    end
    dev_command.say("Deleting " .. remote .. "/" .. branch .. " at " .. tip:gsub("%s+$", "") .. ".")
    dev_os.stdout:flush()
    code = dev_os.run(
        { "jj-git-guard", "push", "--", remote, ":refs/heads/" .. branch },
        { cwd = root }
    )
    if code ~= 0 then
        return code or 1
    end
end
return 0
