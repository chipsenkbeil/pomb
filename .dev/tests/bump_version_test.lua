local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

local function manifest(version)
    return ('[package]\nname = "pomb"\nversion = "%s"\nedition = "2024"\n\n[dependencies.helper]\nversion = "0.5.85"\n'):format(
        version
    )
end

local function lock(version)
    return ('version = 4\n\n[[package]]\nname = "other"\nversion = "0.5.85"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n\n[[package]]\nname = "pomb"\nversion = "%s"\ndependencies = [\n "other",\n]\n\n[[package]]\nname = "extra"\nversion = "9.9.9"\n'):format(
        version
    )
end

local LOCK_WITHOUT_POMB = 'version = 4\n\n[[package]]\nname = "other"\nversion = "0.5.85"\n'

local function tree(manifest_text, lock_text)
    local dir = assert(fs.realpath(testenv.tempdir()))
    assert(fs.symlink(ROOT .. "/.dev", dir .. "/.dev"))
    assert(fs.mkdir(dir .. "/work"))
    assert(fs.write(dir .. "/work/Cargo.toml", manifest_text))
    assert(fs.write(dir .. "/work/Cargo.lock", lock_text))
    return dir
end

local function bump(dir)
    return testenv.verb(dir, { "__bump_version", dir .. "/work" })
end

local function read(dir, name)
    return assert(fs.read(dir .. "/work/" .. name))
end

t.test("bump raises the patch in manifest and lock", function()
    local dir = tree(manifest("0.5.85"), lock("0.5.85"))
    local _, code, err = bump(dir)
    t.eq(code, 0, err)
    t.eq(read(dir, "Cargo.toml"), manifest("0.5.86"))
    t.eq(read(dir, "Cargo.lock"), lock("0.5.86"))
    fs.remove(dir, { recursive = true })
end)

t.test("bump leaves other lock packages and dependency versions alone", function()
    local dir = tree(manifest("0.5.85"), lock("0.5.85"))
    local _, code, err = bump(dir)
    t.eq(code, 0, err)
    t.ok(read(dir, "Cargo.toml"):find('[dependencies.helper]\nversion = "0.5.85"\n', 1, true))
    t.eq(read(dir, "Cargo.lock"), lock("0.5.86"))
    fs.remove(dir, { recursive = true })
end)

t.test("bump prints the new version", function()
    local dir = tree(manifest("0.5.85"), lock("0.5.85"))
    local out, code, err = bump(dir)
    t.eq(code, 0, err)
    t.eq(out, "0.5.86\n")
    fs.remove(dir, { recursive = true })
end)

t.test("bump refuses a lock without a pomb entry", function()
    local dir = tree(manifest("0.5.85"), LOCK_WITHOUT_POMB)
    local _, code, err = bump(dir)
    t.ok(code ~= 0)
    t.ok(err:find("No pomb entry in", 1, true), err)
    t.eq(read(dir, "Cargo.toml"), manifest("0.5.85"))
    t.eq(read(dir, "Cargo.lock"), LOCK_WITHOUT_POMB)
    fs.remove(dir, { recursive = true })
end)

t.test("bump refuses a version that is not three numbers", function()
    local dir = tree(manifest("0.5"), lock("0.5"))
    local _, code, err = bump(dir)
    t.ok(code ~= 0)
    t.ok(err:find("is not MAJOR.MINOR.PATCH", 1, true), err)
    t.eq(read(dir, "Cargo.toml"), manifest("0.5"))
    t.eq(read(dir, "Cargo.lock"), lock("0.5"))
    fs.remove(dir, { recursive = true })
end)

t.test("bump output passes the version guard against the old manifest", function()
    local dir = tree(manifest("0.5.85"), lock("0.5.85"))
    assert(fs.write(dir .. "/base.toml", manifest("0.5.85")))
    local _, code, err = bump(dir)
    t.eq(code, 0, err)
    _, code, err = testenv.verb(
        dir,
        { "__version_guard", dir .. "/base.toml", dir .. "/work/Cargo.toml", dir .. "/work/Cargo.lock" }
    )
    t.eq(code, 0, err)
    fs.remove(dir, { recursive = true })
end)
