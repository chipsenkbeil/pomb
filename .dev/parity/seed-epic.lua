--[[
positional
Seed the "Pomb Epic Test" board, the small board behind the epic card and
Epics panel regress entries.
Usage: dev parity seed-epic BASE_URL COOKIE_JAR
Prints the new board id as the last line of stdout.
Layout: To do holds the epic "Epic Launch" (size 2, so its final-check chip
reads 2, one final-check subtask, the highlighted label "hot") and the open
child "Write the guide"; Done holds the finished child "Draft the outline".
]]
local devos = require("dev.os")
local json = require("dev.json")
local verb = require("dev.verb")

local ctx = ...
local base, jar = ctx.args[1], ctx.args[2]
if not base then
    verb.die("Base url is required. Usage: dev " .. ctx.name .. " BASE_URL COOKIE_JAR.")
end
if not jar then
    verb.die("Cookie jar is required. Usage: dev " .. ctx.name .. " BASE_URL COOKIE_JAR.")
end

local Refused = {}
local ORDER = { "name", "column_id", "kind", "size", "labels" }

local function api(method, path, body)
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

local function get(path)
    return json.decode(api("GET", path))
end

local function seed()
    local board = json.decode(api("POST", "/api/boards", '{"name":"Pomb Epic Test"}')).id
    local cols = get("/api/boards/" .. board .. "/columns")
    local todo = cols[1].id
    local done_col = cols[#cols].id
    api("PATCH", "/api/columns/" .. done_col, '{"is_done":true}')
    api(
        "PUT",
        "/api/boards/" .. board .. "/label-settings",
        '{"highlights":[{"label":"hot","color":"#b8461f"}]}'
    )

    local function mk(fields)
        local body = json.encode(fields, { order = ORDER })
        return json.decode(api("POST", "/api/tasks", body)).id
    end

    local epic = mk({
        name = "Epic Launch",
        column_id = todo,
        kind = "epic",
        size = 2,
        labels = { "hot" },
    })
    api("POST", "/api/tasks/" .. epic .. "/subtasks", '{"name":"Try it live"}')
    local open = mk({ name = "Write the guide", column_id = todo })
    local finished = mk({ name = "Draft the outline", column_id = done_col })
    for _, child in ipairs({ open, finished }) do
        api(
            "POST",
            "/api/tasks/" .. child .. "/links",
            '{"kind":"parent","target_id":"' .. epic .. '"}'
        )
    end
    mk({ name = "Plain card", column_id = todo })
    return board
end

local ok, result = pcall(seed)
if ok then
    io.write(result, "\n")
    return 0
end
if getmetatable(result) == Refused then
    return result.code
end
error(result, 0)
