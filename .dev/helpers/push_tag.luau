---Tag a released commit and push the tag to the remote of record.
---Usage: dev -l push_tag.luau <tag> <sha>. Refuses a tag that already exists here or on the remote, and a commit that is not on its main, so a tag only ever names something that shipped.
local dev_command = require("@dev/command")
local dev_os = require("@dev/os")
local forge = require("@lib/pomb/forge")
local project = require("@dev/project")

local words = { ... }

local tag, rev = words[1], words[2]
if tag == nil or tag == "" or rev == nil or rev == "" then
    dev_command.note("usage: push_tag.luau <tag> <sha>")
    return 1
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
local out
out, code = dev_os.capture(
    { "git", "rev-parse", "-q", "--verify", rev .. "^{commit}" },
    { cwd = root, stderr = "null" }
)
if code ~= 0 then
    dev_command.note("No commit named " .. rev .. " exists.")
    return 1
end
local sha = out:gsub("%s+$", "")
if
    dev_os.run({ "git", "merge-base", "--is-ancestor", sha, remote .. "/main" }, { cwd = root })
    ~= 0
then
    dev_command.note("Commit " .. sha .. " is not on " .. remote .. "/main.")
    return 1
end
local ref = "refs/tags/" .. tag
local here = dev_os.run(
    { "git", "rev-parse", "-q", "--verify", ref },
    { cwd = root, stdout = "null", stderr = "null" }
)
local there = dev_os.run(
    { "git", "ls-remote", "--exit-code", "--tags", remote, ref },
    { cwd = root, stdout = "null", stderr = "null" }
)
if here == 0 or there == 0 then
    dev_command.note("Tag " .. tag .. " already exists.")
    return 1
end

code = dev_os.run(
    { "git", "tag", "-a", tag, sha, "-m", "Pomb " .. tag:gsub("^v", "") },
    { cwd = root }
)
if code ~= 0 then
    return code or 1
end
dev_command.say("Tagged " .. tag .. " at " .. sha .. ".")
dev_os.stdout:flush()
if dev_os.run({ "jj-git-guard", "push", "--", remote, ref }, { cwd = root }) ~= 0 then
    dev_os.run({ "git", "tag", "-d", tag }, { cwd = root, stdout = "null" })
    return 1
end
return 0
