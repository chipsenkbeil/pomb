local dev_command = require("@dev/command")
local devos = require("@dev/os")
local argv = require("@lib/pomb/argv")

return dev_command({
    summary = "Build the release binary.",
    positional = true,
    pass_unknown = true,
    args = { { name = "args", list = true, rest = true, help = "words for cargo build" } },
    run = function(_, args, ctx)
        local cmd = argv.extend({ "cargo", "build", "--release", "--locked" }, args.args or {})
        return devos.run(cmd, { cwd = ctx.root }) or 1
    end,
})
