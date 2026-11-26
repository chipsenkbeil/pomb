local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

local FMT_CHECK_FAILS = [[if [ "$1" = "+nightly" ] && [ "$2" = "fmt" ] && [ "$3" = "--check" ]; then
    echo "Diff in src/main.rs" >&2
    exit 1
fi
exit 0]]

local function stub(root, name, body)
    local path = root .. "/bin/" .. name
    assert(fs.write(path, "#!/bin/sh\n" .. body .. "\n"))
    assert(devos.run({ "chmod", "755", path }) == 0, "chmod failed")
end

local function lint(words, cargo_body)
    local root = assert(fs.realpath(testenv.tempdir()))
    assert(fs.mkdir(root .. "/bin", { parents = true }))
    assert(fs.symlink(ROOT .. "/.dev", root .. "/.dev"))
    stub(root, "cargo", cargo_body or "exit 0")
    local argv = { "lint" }
    for _, word in ipairs(words) do
        argv[#argv + 1] = word
    end
    return testenv.verb(root, argv, { env = { CI = "" } })
end

t.test("lint_should_refuse_a_stray_argument with a flag", function()
    local _, code, err = lint({ "--bogus" })
    t.eq(code, 2, err)
    t.ok(err:find("Unknown flag --bogus.", 1, true), err)
end)

t.test("lint_should_refuse_a_stray_argument with a word", function()
    local _, code, err = lint({ "extra" })
    t.eq(code, 2, err)
    t.ok(
        err:find("Unknown argument extra. This command takes no positional arguments.", 1, true),
        err
    )
end)

t.test("lint_should_name_a_failed_check_and_keep_going", function()
    local out, code, err = lint({}, FMT_CHECK_FAILS)
    t.eq(code, 1, err)
    t.ok(err:find("These checks failed: cargo fmt", 1, true), err)
    t.ok(out:find("> cargo fmt", 1, true), out)
    t.ok(out:find("> cargo clippy", 1, true), out)
end)
