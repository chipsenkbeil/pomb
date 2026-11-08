--[[
positional
Run the test archive in user, mount and network namespaces, Linux only.
]]
local devos = require("dev.os")

local ctx = ...

local function trimmed(out)
    return (out:gsub("\n$", ""))
end

local target = os.getenv("CARGO_TARGET_DIR")
if target == nil or target == "" then
    target = ctx.root .. "/target"
end
local archive = target .. "/nixcheck.tar.zst"

local out, code = devos.capture({ "realpath", "-m", "--", archive }, { cwd = ctx.root })
if code ~= 0 then
    io.stderr:write("Cannot resolve ", archive, ".\n")
    return 1
end
local resolved = trimmed(out)
if resolved == "/var/tmp" or resolved:sub(1, 9) == "/var/tmp/" then
    io.stderr:write(
        archive,
        " is under /var/tmp, which the sandbox covers with a private mount. Point CARGO_TARGET_DIR elsewhere.\n"
    )
    return 1
end

if devos.run({ "unshare", "--user", "--map-root-user", "--mount", "--net", "true" }) ~= 0 then
    io.stderr:write("Unprivileged user, mount and network namespaces are not available here.\n")
    return 1
end

local status = devos.run(
    { "cargo", "nextest", "archive", "--all-features", "--archive-file", archive },
    { cwd = ctx.root }
)
if status ~= 0 then
    return status
end

local nextest = devos.which("cargo-nextest")
if nextest == nil then
    io.stderr:write("cargo-nextest is not on PATH.\n")
    return 1
end

out = devos.capture({ "id", "-u" })
local uid = trimmed(out)
out = devos.capture({ "id", "-g" })
local gid = trimmed(out)

local argv = {
    "unshare",
    "--user",
    "--map-root-user",
    "--mount",
    "--net",
    ctx.root .. "/.dev/standalone/__nixcheck_sandbox.sh",
    archive,
    ctx.root,
    uid,
    gid,
    nextest,
}
for _, arg in ipairs(ctx.args) do
    argv[#argv + 1] = arg
end
local _, message = devos.exec(argv, { cwd = ctx.root })
io.stderr:write(message, "\n")
return devos.EXEC_FAILED
