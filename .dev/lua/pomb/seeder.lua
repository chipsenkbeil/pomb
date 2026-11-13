local devos = require("dev.os")
local json = require("dev.json")
local verb = require("dev.verb")

---@class pomb.seeder
local M = {}

local Refused = {}

---@class pomb.seeder.Client
---@field api fun(method: string, path: string, body: string?): string request that returns the response body or raises when curl refuses
---@field get fun(path: string): any decoded JSON of a GET response
---@field mk fun(fields: table<string, any>): string id of the task created from fields

local function client(base, jar, order)
    local self = {}

    function self.api(method, path, body)
        local argv = { "curl", "-sS", "-f", "-X", method, "-b", jar, "-H", "Accept: application/json" }
        if body then
            for _, word in ipairs({ "-H", "Content-Type: application/json", "--data-binary", body }) do
                argv[#argv + 1] = word
            end
        end
        argv[#argv + 1] = base .. path
        local out, code = devos.capture(argv)
        if code ~= 0 then
            error(setmetatable({ code = code }, Refused), 0)
        end
        return out
    end

    function self.get(path)
        return json.decode(self.api("GET", path))
    end

    function self.mk(fields)
        local body = json.encode(fields, { order = order })
        return json.decode(self.api("POST", "/api/tasks", body)).id
    end

    return self
end

---Run a parity seeder: check its two arguments, run `seed` and print the board id it returns.
---@param ctx dev.Ctx the verb's context, whose arguments are BASE_URL and COOKIE_JAR
---@param order string[] key order `mk` encodes a task with
---@param seed fun(client: pomb.seeder.Client): string builds the board and returns its id
---@return integer code 0, or curl's exit code when a request was refused
function M.run(ctx, order, seed)
    local base, jar = ctx.args[1], ctx.args[2]
    if not base then
        verb.die("Base url is required. Usage: dev " .. ctx.name .. " BASE_URL COOKIE_JAR.")
    end
    if not jar then
        verb.die("Cookie jar is required. Usage: dev " .. ctx.name .. " BASE_URL COOKIE_JAR.")
    end
    local ok, result = pcall(seed, client(base, jar, order))
    if ok then
        io.write(result, "\n")
        return 0
    end
    if getmetatable(result) == Refused then
        return result.code
    end
    error(result, 0)
end

return M
