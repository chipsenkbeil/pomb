local dev_command = require("@dev/command")
local dev_os = require("@dev/os")
local site = require("@lib/pomb/site")

return dev_command({
    summary = "Close port 22 to everything but the tunnel.",
    runs_local = true,
    remote = false,
    positional = true,
    run = function(_opts, _args, ctx)
        local override = dev_os.getenv("POMB_HOST")
        if override ~= nil and override ~= "" then
            dev_command.die(
                "The firewall runs only through " .. site.TUNNEL_HOST .. ", so unset POMB_HOST."
            )
        end
        return site.ssh_script(site.read_script(ctx, "firewall"), { "bash", "-s" })
    end,
})
