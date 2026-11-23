local dev_command = require("@dev/command")
local dev_os = require("@dev/os")
local argv = require("@lib/pomb/argv")

return dev_command({
    summary = "Print the Pomb element that overlaps each golden element of a manifest entry",
    remote = false,
    positional = true,
    flags = {
        {
            name = "state",
            kind = "string",
            required = true,
            help = "State file of the running server",
        },
        { name = "entry", kind = "string", required = true, help = "Id of the manifest entry" },
        { name = "manifest", kind = "string", help = "Manifest file to read" },
    },
    run = function(opts, args, ctx)
        return dev_os.run(argv.node(ctx, "suggest")) or 1
    end,
})
