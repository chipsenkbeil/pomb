--[[
hidden
positional
Refuse a release whose version was not bumped past the base or whose Cargo.lock disagrees.
Usage: dev __version_guard <base Cargo.toml> <candidate Cargo.toml> <candidate Cargo.lock>.
]]
local cargo = require("pomb.cargo")
local fs = require("dev.fs")

local ctx = ...

local base_toml, cand_toml, cand_lock = ctx.args[1], ctx.args[2], ctx.args[3]
if not (base_toml and cand_toml and cand_lock) or base_toml == "" or cand_toml == "" or cand_lock == "" then
    io.stderr:write("usage: __version_guard <base Cargo.toml> <candidate Cargo.toml> <candidate Cargo.lock>\n")
    return 1
end

local new = cargo.manifest_version(assert(fs.read(cand_toml)))
local old = cargo.manifest_version(assert(fs.read(base_toml)))
local locked = cargo.lock_version(assert(fs.read(cand_lock)))
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
