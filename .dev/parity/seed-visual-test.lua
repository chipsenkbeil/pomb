--[[
positional
Seed the "Pomb Visual Test" board, the small board behind the
KanbanFlow timer, modal and card goldens (FQhhSs1).
Usage: dev parity seed-visual-test BASE_URL COOKIE_JAR
Prints the new board id as the last line of stdout.
Layout: Personal To-do holds Task Alpha (green, due
2026-10-02 17:00), Task Beta, Task Delta, Task Epsilon and the long-title
blue task (the modal goldens' task: markdown description, three subtasks);
Do today holds Task Gamma; In progress has WIP limit 3; Done is
the done column.
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
local ORDER = { "name", "column_id", "color_id", "due_at" }
local Color = {
    YELLOW = "yellow",
    GREEN = "green",
    BLUE = "blue",
    RED = "red",
    ORANGE = "orange",
    PURPLE = "purple",
    MAGENTA = "magenta",
    CYAN = "cyan",
}
local PALETTE = {
    Color.YELLOW,
    Color.GREEN,
    Color.BLUE,
    Color.RED,
    Color.ORANGE,
    Color.PURPLE,
    Color.MAGENTA,
    Color.CYAN,
}
local WANTED = {
    { "Personal To-do", "null" },
    { "Do today", "null" },
    { "In progress", "3" },
    { "Done", "null" },
}

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

local function color_id(colors, label)
    for _, color in ipairs(colors) do
        if color.label:lower() == label then
            return color.id
        end
    end
end

local function seed()
    local board = json.decode(api("POST", "/api/boards", '{"name":"Pomb Visual Test"}')).id
    local todo = get("/api/boards/" .. board .. "/columns")[1].id
    local existing = {}
    for i, col in ipairs(get("/api/boards/" .. board .. "/columns")) do
        existing[i] = col.id
    end
    for i, spec in ipairs(WANTED) do
        local name, limit = spec[1], spec[2]
        if existing[i] then
            api(
                "PATCH",
                "/api/columns/" .. existing[i],
                string.format('{"name":"%s","wip_limit":%s,"is_done":false}', name, limit)
            )
        else
            api(
                "POST",
                "/api/columns",
                string.format('{"board_id":"%s","name":"%s","wip_limit":%s}', board, name, limit)
            )
        end
    end
    local cols = get("/api/boards/" .. board .. "/columns")
    local today = cols[2].id
    local done_col = cols[#cols].id
    api("PATCH", "/api/columns/" .. done_col, '{"is_done":true}')

    local colors = get("/api/boards/" .. board .. "/colors")
    local keep = {}
    for _, name in ipairs(PALETTE) do
        keep[name] = true
    end
    for _, color in ipairs(colors) do
        if not keep[color.label:lower()] then
            api("DELETE", "/api/boards/" .. board .. "/colors/" .. color.id)
        end
    end
    for order, name in ipairs(PALETTE) do
        local id = color_id(colors, name)
        api("PATCH", "/api/boards/" .. board .. "/colors/" .. id, '{"sort_order":' .. order .. "}")
    end

    local function mk(fields)
        local body = json.encode(fields, { order = ORDER })
        return json.decode(api("POST", "/api/tasks", body)).id
    end

    local alpha = mk({
        name = "Task Alpha",
        column_id = todo,
        color_id = color_id(colors, Color.GREEN),
        due_at = "2026-10-02 17:00",
    })
    api("PATCH", "/api/tasks/" .. alpha, '{"due_target_column_id":"' .. done_col .. '"}')
    mk({ name = "Task Beta", column_id = todo, color_id = color_id(colors, Color.YELLOW) })
    mk({ name = "Task Delta", column_id = todo, color_id = color_id(colors, Color.YELLOW) })
    mk({ name = "Task Epsilon", column_id = todo, color_id = color_id(colors, Color.YELLOW) })
    local long = mk({
        name = "GM long title: Refactor the authentication middleware so it validates tokens, refreshes sessions and logs every failure for audit",
        column_id = todo,
        color_id = color_id(colors, Color.BLUE),
    })
    api(
        "PATCH",
        "/api/tasks/" .. long,
        [[{"description":"Golden master **bold** and *italic* text.\n\n- item one\n  - item two [link](https://example.com)"}]]
    )
    local created = api("POST", "/api/tasks/" .. long .. "/subtasks", '{"name":"First subtask"}')
    local first = json.decode(created).id
    api("PATCH", "/api/tasks/" .. long .. "/subtasks/" .. first, '{"done":true}')
    api(
        "POST",
        "/api/tasks/" .. long .. "/subtasks",
        '{"name":"Second subtask with a much longer name that should wrap onto another line in the modal"}'
    )
    api("POST", "/api/tasks/" .. long .. "/subtasks", '{"name":"Third subtask"}')
    mk({ name = "Task Gamma", column_id = today, color_id = color_id(colors, Color.YELLOW) })
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
