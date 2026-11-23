local dev_command = require("@dev/command")
local devos = require("@dev/os")

local function trimmed(out)
    return (out:gsub("\n$", ""))
end

return dev_command({
    summary = "Run the test archive in user, mount and network namespaces, Linux only.",
    positional = true,
    pass_unknown = true,
    args = { { name = "args", list = true, rest = true, help = "words for cargo nextest" } },
    run = function(_, args, ctx)
        -- Pick the archive path
        local target = devos.getenv("CARGO_TARGET_DIR")
        if target == nil or target == "" then
            target = ctx.root .. "/target"
        end
        local archive = target .. "/nixcheck.tar.zst"

        -- Refuse an archive under /var/tmp
        local out, code = devos.capture({ "realpath", "-m", "--", archive }, { cwd = ctx.root })
        if code ~= 0 then
            dev_command.note("Cannot resolve " .. archive .. ".")
            return 1
        end
        local resolved = trimmed(out)
        if resolved == "/var/tmp" or resolved:sub(1, 9) == "/var/tmp/" then
            dev_command.note(
                archive
                    .. " is under /var/tmp, which the sandbox covers with a private mount. Point CARGO_TARGET_DIR elsewhere."
            )
            return 1
        end

        -- Check that user namespaces work here
        if
            devos.run({ "unshare", "--user", "--map-root-user", "--mount", "--net", "true" }) ~= 0
        then
            dev_command.note(
                "Unprivileged user, mount and network namespaces are not available here."
            )
            return 1
        end

        -- Build the nextest archive
        local status = devos.run(
            { "cargo", "nextest", "archive", "--all-features", "--archive-file", archive },
            { cwd = ctx.root }
        )
        if status ~= 0 then
            return status
        end

        -- Find cargo-nextest
        local nextest = devos.which("cargo-nextest")
        if nextest == nil then
            dev_command.note("cargo-nextest is not on PATH.")
            return 1
        end

        -- Read the user and group ids
        out = devos.capture({ "id", "-u" })
        local uid = trimmed(out)
        out = devos.capture({ "id", "-g" })
        local gid = trimmed(out)

        -- Run the archive inside the sandbox
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
        for _, arg in ipairs(args.args or {}) do
            argv[#argv + 1] = arg
        end
        local _, message = devos.exec(argv, { cwd = ctx.root })
        dev_command.note(tostring(message))
        return devos.EXEC_FAILED
    end,
})
