local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")
local seeder = require("pomb.seeder")
local verb = require("dev.verb")

local function stub(code)
    local dir = assert(fs.realpath(testenv.tempdir()))
    testenv.fake(dir, "curl", { code = code, out = '{"id":"b1"}' })
    return dir
end

local function run(dir, args, seed)
    local saved = devos.defaults.env
    devos.defaults.env = { PATH = dir .. "/bin:" .. os.getenv("PATH") }
    local written = {}
    local write = io.write
    io.write = function(...)
        for _, text in ipairs({ ... }) do
            written[#written + 1] = text
        end
    end
    local ok, result = pcall(seeder.run, { name = "parity seed-x", args = args }, { "name" }, seed)
    io.write = write
    devos.defaults.env = saved
    return ok, result, table.concat(written)
end

t.test("seeder should print the board id the seed returns", function()
    local dir = stub(0)
    local ok, code, out = run(dir, { "http://x", "jar" }, function(client)
        return client.get("/api/boards").id .. client.mk({ name = "Card" })
    end)
    t.ok(ok, code)
    t.eq(code, 0)
    t.eq(out, "b1b1\n")
    local calls = testenv.calls(dir, "curl")
    local want = "-sS -f -X POST -b jar -H Accept: application/json"
        .. ' -H Content-Type: application/json --data-binary {"name":"Card"} http://x/api/tasks'
    t.eq(calls[2], want)
end)

t.test("seeder should return curl's code when a request is refused", function()
    local dir = stub(22)
    local ok, code, out = run(dir, { "http://x", "jar" }, function(client)
        client.api("GET", "/api/boards")
        return "never"
    end)
    t.ok(ok, code)
    t.eq(code, 22)
    t.eq(out, "")
end)

t.test("seeder should die with the usage when an argument is missing", function()
    local dir = stub(0)
    local die = verb.die
    verb.die = function(msg)
        error(msg, 0)
    end
    local ok, err = run(dir, { "http://x" }, function()
        return "b1"
    end)
    verb.die = die
    t.ok(not ok)
    t.eq(err, "Cookie jar is required. Usage: dev parity seed-x BASE_URL COOKIE_JAR.")
end)
