local dev_command = require("@dev/command")
local site = require("@lib/pomb/site")

return dev_command({
    summary = "Remove the files a verifier planted in the nightly backups directory.",
    hidden = true,
    runs_local = true,
    remote = false,
    positional = true,
    run = function(_opts, _args, ctx)
        return site.ssh_script(site.read_script(ctx, "decoys"), { "bash", "-s" })
    end,
})
