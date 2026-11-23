local dev_command = require("@dev/command")
local site = require("@lib/pomb/site")

return dev_command({
    summary = "Print the setup code each channel logged at its first start.",
    runs_local = true,
    remote = false,
    positional = true,
    run = function(_opts, _args, ctx)
        return site.ssh_script(site.read_script(ctx, "setupcode"), { "bash", "-s" })
    end,
})
