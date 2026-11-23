local dev_command = require("@dev/command")
local site = require("@lib/pomb/site")

return dev_command({
    summary = "Enable and start a channel's service and update timer.",
    runs_local = true,
    remote = false,
    positional = true,
    args = {
        { name = "channel", required = true, help = "stable or nightly" },
    },
    run = function(_opts, args, ctx)
        local channel = args.channel
        if not site.is_channel(channel) then
            dev_command.usage_error(ctx, "The channel must be stable or nightly.")
        end

        local service = "pomb-" .. channel .. ".service"
        local timer = "pomb-update@" .. channel .. ".timer"
        local code = site.ssh({ "systemctl", "enable", "--now", service, timer })
        if code ~= 0 then
            return code
        end
        return site.ssh({ "systemctl", "is-active", service, timer })
    end,
})
