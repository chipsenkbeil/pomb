---Refuse a release whose version was not bumped past the base or whose Cargo.lock disagrees.
---Usage: dev -l version_guard.luau <base Cargo.toml> <candidate Cargo.toml> <candidate Cargo.lock>.
local cargo = require("@lib/pomb/cargo")
local dev_command = require("@dev/command")
local fs = require("@dev/fs")

local words = { ... }

local base_toml, cand_toml, cand_lock = words[1], words[2], words[3]
if
    not (base_toml and cand_toml and cand_lock)
    or base_toml == ""
    or cand_toml == ""
    or cand_lock == ""
then
    dev_command.note(
        "usage: version_guard.luau <base Cargo.toml> <candidate Cargo.toml> <candidate Cargo.lock>"
    )
    return 1
end

local new = cargo.manifest_version(assert(fs.read(cand_toml)))
local old = cargo.manifest_version(assert(fs.read(base_toml)))
local locked = cargo.lock_version(assert(fs.read(cand_lock)))
if not new then
    dev_command.note("The candidate Cargo.toml has no package version.")
    return 1
end
if not old then
    dev_command.note("The base Cargo.toml has no package version.")
    return 1
end
if new == old then
    dev_command.note(
        ("Cargo.toml version %s equals the base version %s, so bump and commit it."):format(
            new,
            old
        )
    )
    return 1
end
if locked ~= new then
    dev_command.note(
        ("Cargo.lock pomb version %s differs from Cargo.toml version %s, so commit the lock update."):format(
            locked or "none",
            new
        )
    )
    return 1
end
return 0
