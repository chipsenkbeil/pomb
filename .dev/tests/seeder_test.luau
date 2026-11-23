local dev_command = require("@dev/command")
local devos = require("@dev/os")
local fs = require("@dev/fs")
local t = require("@dev/testing")
local testenv = require("@dev/testenv")
local seeder = require("@lib/pomb/seeder")

local function stub(code)
    local dir = assert(fs.realpath(testenv.tempdir()))
    testenv.fake(dir, "curl", { code = code, out = '{"id":"b1"}' })
    return dir
end

local function run(dir, args, seed)
    local saved = devos.defaults.env
    devos.defaults.env = { PATH = dir .. "/bin:" .. devos.getenv("PATH") }
    local written = {}
    local say = dev_command.say
    dev_command.say = function(text)
        written[#written + 1] = text .. "\n"
    end
    local ok, result = pcall(seeder.run, args, { "name" }, seed)
    dev_command.say = say
    devos.defaults.env = saved
    return ok, result, table.concat(written)
end

t.test("seeder should print the board id the seed returns", function()
    local dir = stub(0)
    local args = { base_url = "http://x", cookie_jar = "jar" }
    local ok, code, out = run(dir, args, function(client)
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
    local args = { base_url = "http://x", cookie_jar = "jar" }
    local ok, code, out = run(dir, args, function(client)
        client.api("GET", "/api/boards")
        return "never"
    end)
    t.ok(ok, code)
    t.eq(code, 22)
    t.eq(out, "")
end)
