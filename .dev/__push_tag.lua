--[[
hidden
positional
Tag a released commit and push the tag to the remote of record.
Usage: dev __push_tag <tag> <sha>. Refuses a tag that already exists here or on the remote, and a commit that is not on its main, so a tag only ever names something that shipped.
]]
local devos = require("dev.os")
local forge = require("pomb.forge")

local ctx = ...

local tag, rev = ctx.args[1], ctx.args[2]
if tag == nil or tag == "" or rev == nil or rev == "" then
    io.stderr:write("usage: __push_tag.sh <tag> <sha>\n")
    return 1
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
local out
out, code = devos.capture(
    { "git", "rev-parse", "-q", "--verify", rev .. "^{commit}" },
    { cwd = ctx.root, stderr = "null" }
)
if code ~= 0 then
    io.stderr:write("No commit named ", rev, " exists.\n")
    return 1
end
local sha = out:gsub("%s+$", "")
if
    devos.run({ "git", "merge-base", "--is-ancestor", sha, remote .. "/main" }, { cwd = ctx.root })
    ~= 0
then
    io.stderr:write("Commit ", sha, " is not on ", remote, "/main.\n")
    return 1
end
local ref = "refs/tags/" .. tag
local here = devos.run(
    { "git", "rev-parse", "-q", "--verify", ref },
    { cwd = ctx.root, stdout = "null", stderr = "null" }
)
local there = devos.run(
    { "git", "ls-remote", "--exit-code", "--tags", remote, ref },
    { cwd = ctx.root, stdout = "null", stderr = "null" }
)
if here == 0 or there == 0 then
    io.stderr:write("Tag ", tag, " already exists.\n")
    return 1
end

code = devos.run(
    { "git", "tag", "-a", tag, sha, "-m", "Pomb " .. tag:gsub("^v", "") },
    { cwd = ctx.root }
)
if code ~= 0 then
    return code or 1
end
io.stdout:write("Tagged ", tag, " at ", sha, ".\n")
io.stdout:flush()
if devos.run({ "jj-git-guard", "push", "--", remote, ref }, { cwd = ctx.root }) ~= 0 then
    devos.run({ "git", "tag", "-d", tag }, { cwd = ctx.root, stdout = "null" })
    return 1
end
return 0
