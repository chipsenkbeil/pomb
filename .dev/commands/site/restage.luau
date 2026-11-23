local dev_command = require("@dev/command")
local site = require("@lib/pomb/site")

return dev_command({
    summary = "Make the next update real.",
    runs_local = true,
    remote = false,
    positional = true,
    flags = {
        { name = "bad", kind = "boolean", help = "stage a release that fails to start" },
    },
    args = {
        { name = "channel", required = true, help = "only nightly" },
    },
    run = function(opts, args, ctx)
        if args.channel ~= site.CHANNEL.NIGHTLY then
            dev_command.usage_error(ctx, "Only the nightly channel can be restaged.")
        end

        local script = site.read_script(ctx, "restage")
        return site.ssh_script(script, { "BAD=" .. (opts.bad and "1" or ""), "bash", "-s" })
    end,
})
