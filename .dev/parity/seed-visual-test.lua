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
local json = require("dev.json")
local seeder = require("pomb.seeder")

local ctx = ...
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
    { "Personal To-do", json.null },
    { "Do today", json.null },
    { "In progress", 3 },
    { "Done", json.null },
}
local COLUMN_ORDER = { "board_id", "name", "wip_limit", "is_done" }

local function color_id(colors, label)
    for _, color in ipairs(colors) do
        if color.label:lower() == label then
            return color.id
        end
    end
end

local function seed(client)
    local api, get, mk = client.api, client.get, client.mk
    local board = json.decode(api("POST", "/api/boards", '{"name":"Pomb Visual Test"}')).id
    local todo = get("/api/boards/" .. board .. "/columns")[1].id
    local existing = {}
    for i, col in ipairs(get("/api/boards/" .. board .. "/columns")) do
        existing[i] = col.id
    end
    for i, spec in ipairs(WANTED) do
        local name, limit = spec[1], spec[2]
        if existing[i] then
            local body = json.encode(
                { name = name, wip_limit = limit, is_done = false },
                { order = COLUMN_ORDER }
            )
            api("PATCH", "/api/columns/" .. existing[i], body)
        else
            local body = json.encode(
                { board_id = board, name = name, wip_limit = limit },
                { order = COLUMN_ORDER }
            )
            api("POST", "/api/columns", body)
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

return seeder.run(ctx, ORDER, seed)
