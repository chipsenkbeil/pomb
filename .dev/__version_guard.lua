--[[
hidden
positional
Refuse a release whose version was not bumped past the base or whose Cargo.lock disagrees.
Usage: dev __version_guard <base Cargo.toml> <candidate Cargo.toml> <candidate Cargo.lock>.
]]
local fs = require("dev.fs")

local ctx = ...

local base_toml, cand_toml, cand_lock = ctx.args[1], ctx.args[2], ctx.args[3]
if not (base_toml and cand_toml and cand_lock) or base_toml == "" or cand_toml == "" or cand_lock == "" then
    io.stderr:write("usage: __version_guard <base Cargo.toml> <candidate Cargo.toml> <candidate Cargo.lock>\n")
    return 1
end

local function manifest_version(path)
    local in_package = false
    for line in (assert(fs.read(path)) .. "\n"):gmatch("(.-)\n") do
        if line:sub(1, 1) == "[" then
            in_package = line == "[package]"
        end
        local version = in_package and line:match('^version = "(.*)"$')
        if version then
            return version
        end
    end
end

local function lock_version(path)
    local found = false
    for line in (assert(fs.read(path)) .. "\n"):gmatch("(.-)\n") do
        if line == 'name = "pomb"' then
            found = true
        elseif found then
            local version = line:match('^version = "(.*)"$')
            if version then
                return version
            end
        end
    end
end

local new = manifest_version(cand_toml)
local old = manifest_version(base_toml)
local locked = lock_version(cand_lock)
if not new then
    io.stderr:write("The candidate Cargo.toml has no package version.\n")
    return 1
end
if not old then
    io.stderr:write("The base Cargo.toml has no package version.\n")
    return 1
end
if new == old then
    io.stderr:write(("Cargo.toml version %s equals the base version %s, so bump and commit it.\n"):format(new, old))
    return 1
end
if locked ~= new then
    io.stderr:write(
        ("Cargo.lock pomb version %s differs from Cargo.toml version %s, so commit the lock update.\n"):format(
            locked or "none",
            new
        )
    )
    return 1
end
return 0
