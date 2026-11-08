--[[
hidden
positional
Raise the patch number of the pomb version in Cargo.toml and Cargo.lock and print the new version.
Usage: dev __bump_version <dir holding Cargo.toml and Cargo.lock>. Refuses a lock without a pomb entry or a version that is not MAJOR.MINOR.PATCH before it writes either file.
]]
local fs = require("dev.fs")

local ctx = ...

local dir = ctx.args[1]
if dir == nil or dir == "" then
    io.stderr:write("usage: __bump_version <dir holding Cargo.toml and Cargo.lock>\n")
    return 1
end
local manifest = dir .. "/Cargo.toml"
local lock = dir .. "/Cargo.lock"

local function lines_of(path)
    local text = assert(fs.read(path))
    local lines = {}
    for line in (text .. "\n"):gmatch("(.-)\n") do
        lines[#lines + 1] = line
    end
    if text:sub(-1) == "\n" then
        lines[#lines] = nil
    end
    return lines, text:sub(-1) == "\n"
end

local function manifest_line(lines)
    local in_package = false
    for n, line in ipairs(lines) do
        if line:sub(1, 1) == "[" then
            in_package = line == "[package]"
        end
        if in_package and line:find('^version = "') then
            return n
        end
    end
end

local function lock_line(lines)
    local found = false
    for n, line in ipairs(lines) do
        if line:find("^%[%[package%]%]") then
            found = false
        end
        if line == 'name = "pomb"' then
            found = true
        elseif found and line:find('^version = "') then
            return n
        end
    end
end

local function save(path, lines, n, version, newline)
    lines[n] = lines[n]:gsub('"[^"]*"', '"' .. version .. '"', 1)
    assert(fs.write(path, table.concat(lines, "\n") .. (newline and "\n" or "")))
end

local manifest_lines, manifest_newline = lines_of(manifest)
local lock_lines, lock_newline = lines_of(lock)
local manifest_ln = manifest_line(manifest_lines)
if not manifest_ln then
    io.stderr:write(("No package version in %s.\n"):format(manifest))
    return 1
end
local lock_ln = lock_line(lock_lines)
if not lock_ln then
    io.stderr:write(("No pomb entry in %s.\n"):format(lock))
    return 1
end

local old = manifest_lines[manifest_ln]:match('^version = "([^"]*)"')
local major, minor, patch = old:match("^(%d+)%.(%d+)%.(%d+)$")
if not major then
    io.stderr:write(("Version %s in %s is not MAJOR.MINOR.PATCH.\n"):format(old, manifest))
    return 1
end
local new = ("%s.%s.%d"):format(major, minor, tonumber(patch) + 1)

save(manifest, manifest_lines, manifest_ln, new, manifest_newline)
save(lock, lock_lines, lock_ln, new, lock_newline)
io.stdout:write(new, "\n")
return 0
