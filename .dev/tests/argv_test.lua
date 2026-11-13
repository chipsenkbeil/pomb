local argv = require("pomb.argv")
local t = require("dev.testing")

t.test("extend appends each word and returns the same list", function()
    local list = { "cargo", "build" }
    t.eq(argv.extend(list, { "--release", "-j", "2" }), { "cargo", "build", "--release", "-j", "2" })
    t.eq(list, { "cargo", "build", "--release", "-j", "2" })
end)

t.test("extend with no words leaves the list as it is", function()
    t.eq(argv.extend({ "cargo" }, {}), { "cargo" })
end)

t.test("node builds the node command for a parity script", function()
    local ctx = { root = "/repo", args = { "--candidate", "x" } }
    t.eq(argv.node(ctx, "finder"), { "node", "/repo/.dev/parity/finder.mjs", "--candidate", "x" })
end)

t.test("node with no arguments is the bare command", function()
    t.eq(argv.node({ root = "/repo", args = {} }, "run"), { "node", "/repo/.dev/parity/run.mjs" })
end)

t.test("node keeps an argument with a space as one word", function()
    local ctx = { root = "/repo", args = { "two words" } }
    t.eq(argv.node(ctx, "suggest"), { "node", "/repo/.dev/parity/suggest.mjs", "two words" })
end)
