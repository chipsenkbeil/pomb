local dev_command = require("@dev/command")
local dev_os = require("@dev/os")
local argv = require("@lib/pomb/argv")

return dev_command({
    summary = "Replay the parity manifest on a running Pomb and compare it with the goldens",
    remote = false,
    positional = true,
    flags = {
        {
            name = "state",
            kind = "string",
            required = true,
            help = "State file of the running server",
        },
        { name = "manifest", kind = "string", help = "Manifest file to replay" },
        { name = "out", kind = "string", help = "Report file to write" },
        { name = "only", kind = "string", help = "Entry ids to run, separated by commas" },
        {
            name = "debug-dir",
            kind = "string",
            help = "Folder for screenshots of broken entries",
        },
    },
    run = function(opts, args, ctx)
        return dev_os.run(argv.node(ctx, "run")) or 1
    end,
})
