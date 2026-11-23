local dev_command = require("@dev/command")
local site = require("@lib/pomb/site")

return dev_command({
    summary = "Run the box's pull agent now.",
    runs_local = true,
    remote = false,
    positional = true,
    args = {
        { name = "channel", required = true, help = "stable or nightly" },
        { name = "tag", help = "the release tag to pull" },
    },
    run = function(_opts, args, ctx)
        local channel = args.channel
        if not site.is_channel(channel) then
            dev_command.usage_error(ctx, "The channel must be stable or nightly.")
        end

        local words = { "/usr/local/sbin/pomb-update", channel }
        local tag = args.tag or ""
        if tag ~= "" then
            words[#words + 1] = tag
        end
        return site.ssh(words)
    end,
})
