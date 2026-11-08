local cargo = require("pomb.cargo")
local t = require("dev.testing")

t.test("manifest_version reads the version under [package]", function()
    local text = '[package]\nname = "pomb"\nversion = "0.5.82"\nedition = "2024"\n'
    t.eq(cargo.manifest_version(text), "0.5.82")
end)

t.test("manifest_version skips a version outside [package]", function()
    local text = '[dependencies]\nversion = "9.9.9"\n\n[package]\nname = "pomb"\nversion = "0.5.82"\n'
    t.eq(cargo.manifest_version(text), "0.5.82")
end)

t.test("manifest_version is nil when [package] has no version", function()
    local text = '[dependencies]\nversion = "9.9.9"\n\n[package]\nname = "pomb"\n'
    t.eq(cargo.manifest_version(text), nil)
end)

t.test("lock_version reads the version of the pomb entry", function()
    local text = '[[package]]\nname = "other"\nversion = "9.9.9"\n\n[[package]]\nname = "pomb"\nversion = "0.5.82"\n'
    t.eq(cargo.lock_version(text), "0.5.82")
end)

t.test("lock_version is nil when the lock has no pomb entry", function()
    local text = '[[package]]\nname = "other"\nversion = "9.9.9"\n'
    t.eq(cargo.lock_version(text), nil)
end)
