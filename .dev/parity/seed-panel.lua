--[[
positional
Seed the "Pomb Panel Test" board, the small board behind the bare card, full
card and Done card panel regress entries.
Usage: dev parity seed-panel BASE_URL COOKIE_JAR
Prints the new board id as the last line of stdout.
Layout: To do holds "Bare card", which has no owner, seats, labels or
metadata, and "Full card", which has an owner, the reviewer seat "code", the
label "ready" and the metadata field "team". The last column is the Done
column and holds "Done card", which has no owner and the reviewer seats "code",
approved at revision "r1", and "docs", approved.
]]
local json = require("dev.json")
local seeder = require("pomb.seeder")

local ctx = ...
local ORDER = { "name", "column_id", "labels", "metadata" }

local function seed(client)
    local api, get, mk = client.api, client.get, client.mk
    local board = json.decode(api("POST", "/api/boards", '{"name":"Pomb Panel Test"}')).id
    local cols = get("/api/boards/" .. board .. "/columns")
    local todo = cols[1].id
    local done_col = cols[#cols].id
    api("PATCH", "/api/columns/" .. done_col, '{"is_done":true}')

    mk({ name = "Bare card", column_id = todo })
    local full = mk({
        name = "Full card",
        column_id = todo,
        labels = { "ready" },
        metadata = { team = "platform" },
    })
    api("POST", "/api/tasks/" .. full .. "/owner", "{}")
    api("POST", "/api/tasks/" .. full .. "/seats", '{"name":"code","role":"reviewer"}')
    local done = mk({ name = "Done card", column_id = todo })
    for _, seat in ipairs({ "code", "docs" }) do
        local seat_body =
            json.encode({ name = seat, role = "reviewer" }, { order = { "name", "role" } })
        api("POST", "/api/tasks/" .. done .. "/seats", seat_body)
        api("POST", "/api/tasks/" .. done .. "/seats/take", json.encode({ seat = seat }))
    end
    local verdict = json.encode(
        { seat = "code", verdict = "approved", revision = "r1" },
        { order = { "seat", "verdict", "revision" } }
    )
    api("POST", "/api/tasks/" .. done .. "/seats/verdict", verdict)
    local docs_verdict = json.encode({ seat = "docs", verdict = "approved" })
    api("POST", "/api/tasks/" .. done .. "/seats/verdict", docs_verdict)
    api("POST", "/api/tasks/" .. done .. "/move", json.encode({ column_id = done_col }))
    return board
end

return seeder.run(ctx, ORDER, seed)
