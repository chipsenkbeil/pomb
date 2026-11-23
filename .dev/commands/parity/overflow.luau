local dev_command = require("@dev/command")
local dev_os = require("@dev/os")
local argv = require("@lib/pomb/argv")

return dev_command({
    summary = "Check that the candidate pages do not scroll sideways at a phone width",
    remote = false,
    positional = true,
    args = {
        { name = "state_file", required = true, help = "State file of the candidate server" },
        { name = "width", required = true, help = "Viewport width in pixels" },
    },
    run = function(opts, args, ctx)
        return dev_os.run(argv.node(ctx, "overflow")) or 1
    end,
})
