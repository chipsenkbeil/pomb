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
local json = require("dev.json")
local seeder = require("pomb.seeder")

local ctx = ...
local ORDER = { "name", "column_id", "kind", "size", "labels" }

local function seed(client)
    local api, get, mk = client.api, client.get, client.mk
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

return seeder.run(ctx, ORDER, seed)
