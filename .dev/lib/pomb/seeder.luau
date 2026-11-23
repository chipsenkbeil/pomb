local dev_command = require("@dev/command")
local devos = require("@dev/os")
local json = require("@dev/json")

---@class pomb.seeder
local M = {}

local Refused = {}

---@class pomb.seeder.Client
---@field api fun(method: string, path: string, body: string?): string request that returns the response body or raises when curl refuses
---@field get fun(path: string): any decoded JSON of a GET response
---@field mk fun(fields: table<string, any>): string id of the task created from fields

local function client(base, jar, order)
    -- Make the client table
    local self = {}

    -- Send one request through curl
    function self.api(method, path, body)
        local argv =
            { "curl", "-sS", "-f", "-X", method, "-b", jar, "-H", "Accept: application/json" }
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

    -- Fetch and decode a path
    function self.get(path)
        return json.decode(self.api("GET", path))
    end

    -- Create a task and return its id
    function self.mk(fields)
        local body = json.encode(fields, { order = order })
        return json.decode(self.api("POST", "/api/tasks", body)).id
    end

    -- Hand back the client
    return self
end

---Run a parity seeder: run `seed` and print the board id it returns.
---@param args table<string, string> the command's arguments, `base_url` and `cookie_jar`
---@param order string[] key order `mk` encodes a task with
---@param seed fun(client: pomb.seeder.Client): string builds the board and returns its id
---@return integer code 0, or curl's exit code when a request was refused
function M.run(args, order, seed)
    local ok, result = pcall(seed, client(args.base_url, args.cookie_jar, order))
    if ok then
        dev_command.say(result)
        return 0
    end
    if getmetatable(result) == Refused then
        return result.code
    end
    error(result, 0)
end

return M
