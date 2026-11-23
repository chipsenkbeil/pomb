local dev_command = require("@dev/command")
local dev_os = require("@dev/os")
local argv = require("@lib/pomb/argv")

return dev_command({
    summary = "Check the task finder of a candidate server against its state file",
    remote = false,
    positional = true,
    flags = {
        { name = "candidate", kind = "string", help = "State file of the candidate server" },
    },
    run = function(opts, args, ctx)
        return dev_os.run(argv.node(ctx, "finder")) or 1
    end,
})
