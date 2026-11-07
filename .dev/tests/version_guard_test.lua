local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

local function manifest(version)
    return ('[package]\nname = "pomb"\nversion = "%s"\nedition = "2024"\n'):format(version)
end

local function lock(version)
    return ('[[package]]\nname = "other"\nversion = "9.9.9"\n\n[[package]]\nname = "pomb"\nversion = "%s"\n'):format(
        version
    )
end

local function guard(base, cand_manifest, cand_lock)
    local dir = assert(fs.realpath(testenv.tempdir()))
    assert(fs.symlink(ROOT .. "/.dev", dir .. "/.dev"))
    assert(fs.write(dir .. "/base.toml", manifest(base)))
    assert(fs.write(dir .. "/cand.toml", manifest(cand_manifest)))
    assert(fs.write(dir .. "/cand.lock", lock(cand_lock)))
    local _, code, err =
        testenv.verb(dir, { "__version_guard", dir .. "/base.toml", dir .. "/cand.toml", dir .. "/cand.lock" })
    fs.remove(dir, { recursive = true })
    return code, err
end

t.test("guard accepts a bumped version with a matching lock", function()
    local code, err = guard("0.5.81", "0.5.82", "0.5.82")
    t.eq(code, 0, err)
end)

t.test("guard refuses a version equal to the base", function()
    local code, err = guard("0.5.81", "0.5.81", "0.5.81")
    t.ok(code ~= 0)
    t.ok(err:find("Cargo.toml version 0.5.81 equals the base version 0.5.81", 1, true), err)
end)

t.test("guard refuses a lock that differs from the manifest", function()
    local code, err = guard("0.5.81", "0.5.82", "0.5.81")
    t.ok(code ~= 0)
    t.ok(err:find("Cargo.lock pomb version 0.5.81 differs from Cargo.toml version 0.5.82", 1, true), err)
end)
