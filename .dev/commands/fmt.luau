--!strict
local dev_command = require("@dev/command")
local dev_os = require("@dev/os")
local lintset = require("@lib/lintset")

type Ctx = {
    ---absolute path of the project root
    root: string,
    ---verbosity from 0 to 4
    verbose: number,
}

return dev_command({
    summary = "Format the Rust workspace and the Lua under .dev and .ci",
    long = [[
Nightly rustfmt formats the Rust workspace, stylua formats the Lua under `.dev` and `.ci`, and each
`//` one-liner then loses its trailing period. Nightly is needed for the unstable rustfmt.toml
options.]],
    group = "Check your work",
    patch = { job = "fmt", file = "fmt.patch" },
    run = function(_: unknown, _: unknown, ctx: Ctx): number
        local root = ctx.root

        local code = dev_os.run({ "cargo", "+nightly", "fmt" }, { cwd = root })
        if code ~= 0 then
            return code or 1
        end

        local run = lintset.new(ctx)
        local dev_lua = run:dev_lua()
        if #dev_lua > 0 then
            local argv = lintset.with_files({ "stylua", "--config-path", "stylua.toml" }, dev_lua)
            code = dev_os.run(argv, { cwd = root })
            if code ~= 0 then
                return code or 1
            end
        end

        for _, file in ipairs(run:files({ "*.rs" })) do
            local ok, err = lintset.fix_comment_periods(root, file)
            if not ok then
                dev_command.die(tostring(err))
            end
        end
        return 0
    end,
})
