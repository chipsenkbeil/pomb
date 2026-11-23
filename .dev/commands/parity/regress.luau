local dev_command = require("@dev/command")
local dev_os = require("@dev/os")
local argv = require("@lib/pomb/argv")

return dev_command({
    summary = "Diff the screenshots of two running Pomb builds entry by entry",
    remote = false,
    positional = true,
    flags = {
        {
            name = "candidate",
            kind = "string",
            required = true,
            help = "State file of the candidate server",
        },
        {
            name = "baseline",
            kind = "string",
            required = true,
            help = "State file of the baseline server",
        },
        {
            name = "out",
            kind = "string",
            required = true,
            help = "Folder for the report and diff images",
        },
        { name = "manifest", kind = "string", help = "Manifest file to replay" },
        { name = "only", kind = "string", help = "Entry ids to run, separated by commas" },
        {
            name = "max-diff",
            kind = "number",
            help = "Pixels that may differ before an entry fails",
        },
        { name = "fuzz", kind = "string", help = "Per-channel tolerance, such as 1%" },
    },
    run = function(opts, args, ctx)
        return dev_os.run(argv.node(ctx, "regress")) or 1
    end,
})
