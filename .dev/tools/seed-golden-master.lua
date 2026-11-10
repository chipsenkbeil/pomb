--[[
group: Parity
positional
local
flags: --as-of
Reproduce the KanbanFlow Golden Master board on a Pomb instance
usage: dev tools seed-golden-master [--as-of YYYY-MM-DD] BASE_URL COOKIE_JAR [FIXTURE_JSON]
BASE_URL is the server, never production. COOKIE_JAR is a curl cookie jar sent with every request, which may be empty against a server without sign-in. FIXTURE_JSON defaults to .dev/golden-master-board.json.
It creates a new board and leaves existing boards alone, prints its id as the last line of standard output, and writes the progress and the fixture fields the API cannot reproduce to standard error.
The fixture was captured on 2026-09-30. Every fixture date shifts by the whole days between that day and --as-of, which defaults to today, so each task keeps its distance from today. Times of day are kept and read in the server's timezone. Pass --as-of 2026-09-30 to seed the literal dates.
The last fixture column becomes the Done column. Its tasks are created in the first column and moved in, so Pomb stamps a completion time. Columns, colors and members resolve by name from API responses, and board colors absent from the fixture palette are deleted. An assignee with no matching username falls back to the only registered user. logged_hours becomes one manual time entry starting 09:00 on the day before --as-of.
]]
local devos = require("dev.os")
local fs = require("dev.fs")
local json = require("dev.json")
local verb = require("dev.verb")

local ctx = ...

local CAPTURE_DATE = "2026-09-30"
local MAX_LOGGED_MINUTES = 15 * 60
local LOG_START_HOUR = 9

local function log(message)
    io.stderr:write(message, "\n")
end

local function truthy(value)
    return value ~= nil and value ~= false and value ~= json.null
end

local function days_from_civil(year, month, day)
    if month <= 2 then
        year = year - 1
    end
    local era = math.floor(year / 400)
    local year_of_era = year - era * 400
    local shifted_month = month > 2 and month - 3 or month + 9
    local day_of_year = math.floor((153 * shifted_month + 2) / 5) + day - 1
    local day_of_era = year_of_era * 365
        + math.floor(year_of_era / 4)
        - math.floor(year_of_era / 100)
        + day_of_year
    return era * 146097 + day_of_era - 719468
end

local function civil_from_days(days)
    days = days + 719468
    local era = math.floor(days / 146097)
    local day_of_era = days - era * 146097
    local year_of_era = math.floor(
        (
            day_of_era
            - math.floor(day_of_era / 1460)
            + math.floor(day_of_era / 36524)
            - math.floor(day_of_era / 146096)
        ) / 365
    )
    local day_of_year = day_of_era
        - (365 * year_of_era + math.floor(year_of_era / 4) - math.floor(year_of_era / 100))
    local shifted_month = math.floor((5 * day_of_year + 2) / 153)
    local day = day_of_year - math.floor((153 * shifted_month + 2) / 5) + 1
    local month = shifted_month < 10 and shifted_month + 3 or shifted_month - 9
    local year = year_of_era + era * 400 + (month <= 2 and 1 or 0)
    return year, month, day
end

local function parse_date(text)
    local year, month, day = text:match("^(%d%d%d%d)-(%d%d)-(%d%d)$")
    if not year then
        return nil
    end
    local days = days_from_civil(tonumber(year), tonumber(month), tonumber(day))
    local back_year, back_month, back_day = civil_from_days(days)
    if string.format("%04d-%02d-%02d", back_year, back_month, back_day) ~= text then
        return nil
    end
    return days
end

local function format_date(days)
    return string.format("%04d-%02d-%02d", civil_from_days(days))
end

local function parse_arguments()
    local as_of = os.date("%Y-%m-%d")
    local positional = {}
    local args = ctx.args
    local i = 1
    while i <= #args do
        local arg = args[i]
        if arg == "--as-of" then
            if i == #args then
                verb.usage_error(ctx, "The --as-of flag takes a date.")
            end
            as_of = args[i + 1]
            i = i + 2
        elseif arg:match("^%-%-as%-of=") then
            as_of = arg:sub(#"--as-of=" + 1)
            i = i + 1
        else
            positional[#positional + 1] = arg
            i = i + 1
        end
    end
    if #positional < 2 or #positional > 3 then
        verb.usage_error(ctx, "Pass BASE_URL and COOKIE_JAR, then optionally FIXTURE_JSON.")
    end
    return as_of, positional
end

local as_of, positional = parse_arguments()
local base_url = positional[1]:gsub("/+$", "")
local cookie_jar = positional[2]
local fixture_path = positional[3] or (ctx.root .. "/.dev/golden-master-board.json")

local as_of_days = parse_date(as_of)
if not as_of_days then
    verb.die("--as-of must be YYYY-MM-DD, got '" .. as_of .. "'.")
end
if not fs.is_file(cookie_jar) then
    verb.die("The cookie jar is not readable: " .. cookie_jar)
end

local fixture_text = fs.read(fixture_path)
if not fixture_text then
    verb.die("The fixture is not readable: " .. fixture_path)
end
local fixture = json.decode(fixture_text)
if
    type(fixture) ~= "table"
    or not truthy(fixture.board_name)
    or type(fixture.columns) ~= "table"
    or #fixture.columns == 0
    or type(fixture.tasks) ~= "table"
then
    verb.die("The fixture lacks board_name, columns and tasks: " .. fixture_path)
end

local capture_days = parse_date(CAPTURE_DATE)
local shift_days = as_of_days - capture_days
log(string.format("As of %s, shifting fixture dates by %d day(s).", as_of, shift_days))

local function api(method, path, body)
    local argv = {
        "curl",
        "-sS",
        "-X",
        method,
        "-b",
        cookie_jar,
        "-H",
        "Accept: application/json",
        "-w",
        "\n%{http_code}",
    }
    local output, code
    if body ~= nil then
        local encoded, message = json.encode(body)
        if not encoded then
            verb.die(
                "The request " .. method .. " " .. path .. " could not encode its body: " .. message
            )
        end
        argv[#argv + 1] = "-H"
        argv[#argv + 1] = "Content-Type: application/json"
        argv[#argv + 1] = "--data-binary"
        argv[#argv + 1] = "@-"
        argv[#argv + 1] = base_url .. path
        output, code = devos.capture_input(argv, encoded)
    else
        argv[#argv + 1] = base_url .. path
        output, code = devos.capture(argv)
    end
    if code ~= 0 then
        verb.die("The request " .. method .. " " .. path .. " failed in curl.")
    end
    local split = output:match("^.*()\n")
    local status = split and output:sub(split + 1) or ""
    local text = split and output:sub(1, split - 1) or output
    if not status:match("^2%d%d$") then
        verb.die(
            "The request " .. method .. " " .. path .. " answered HTTP " .. status .. ". " .. text
        )
    end
    if method == "DELETE" or text == "" then
        return nil
    end
    local value, message = json.decode(text)
    if value == nil then
        verb.die("The request " .. method .. " " .. path .. " answered invalid JSON. " .. message)
    end
    return value
end

local function shift_due(value)
    local day, time = value:match("^([^T]+)T?(.*)$")
    local days = parse_date(day)
    if not days then
        verb.die("The fixture date '" .. value .. "' is not YYYY-MM-DD.")
    end
    local shifted = format_date(days + shift_days)
    if time ~= "" then
        return shifted .. " " .. time
    end
    return shifted
end

local function find_by_name(list, name)
    for _, item in ipairs(list) do
        if item.name == name then
            return item
        end
    end
    return nil
end

local board_name = fixture.board_name
local board_id = api("POST", "/api/boards", { name = board_name }).id
log(string.format("Created the board '%s' (%s).", board_name, board_id))

local columns = api("GET", "/api/boards/" .. board_id .. "/columns")
local existing_count = #columns
if existing_count < 1 then
    verb.die("The new board has no columns.")
end
local column_count = #fixture.columns

for i, spec in ipairs(fixture.columns) do
    local wip_limit = truthy(spec.wip_limit) and spec.wip_limit or json.null
    if i <= existing_count then
        api("PATCH", "/api/columns/" .. columns[i].id, {
            name = spec.name,
            is_done = false,
            wip_limit = wip_limit,
        })
    else
        local body = { board_id = board_id, name = spec.name }
        if truthy(spec.wip_limit) then
            body.wip_limit = spec.wip_limit
        end
        api("POST", "/api/columns", body)
    end
end

for i = column_count + 1, existing_count do
    api("DELETE", "/api/columns/" .. columns[i].id)
end

columns = api("GET", "/api/boards/" .. board_id .. "/columns")
local names = {}
local wanted_names = {}
for i, column in ipairs(columns) do
    names[i] = column.name
end
for i, spec in ipairs(fixture.columns) do
    wanted_names[i] = spec.name
end
if table.concat(names, "\0") ~= table.concat(wanted_names, "\0") then
    verb.die("The column order does not match after creation: " .. table.concat(names, ", "))
end

for _, spec in ipairs(fixture.columns) do
    if truthy(spec.card_properties) then
        local config, message = json.encode(spec.card_properties)
        if not config then
            verb.die("The column card properties could not be encoded: " .. message)
        end
        api(
            "PATCH",
            "/api/columns/" .. find_by_name(columns, spec.name).id,
            { config_json = config }
        )
    end
end
local done_column = columns[#columns].id
local first_column = columns[1].id
api("PATCH", "/api/columns/" .. done_column, { is_done = true })
log(string.format("The columns are %s, and the last one is Done.", table.concat(names, " | ")))

local colors = api("GET", "/api/boards/" .. board_id .. "/colors")
local color_map = {}
local kept = {}
local missing = {}
for _, want in ipairs(fixture.colors) do
    local lowered = want:lower()
    local found
    for _, color in ipairs(colors) do
        if color.label:lower() == lowered or color.value == lowered then
            found = color.id
            break
        end
    end
    if found then
        color_map[want] = found
        kept[found] = true
    else
        missing[#missing + 1] = want
    end
end
if #missing > 0 then
    verb.die("Fixture colors are not in the board palette: " .. table.concat(missing, ", "))
end

for _, color in ipairs(colors) do
    if not kept[color.id] then
        api("DELETE", "/api/boards/" .. board_id .. "/colors/" .. color.id)
    end
end

for order, want in ipairs(fixture.colors) do
    api(
        "PATCH",
        "/api/boards/" .. board_id .. "/colors/" .. color_map[want],
        { sort_order = order }
    )
end
log("The palette is " .. table.concat(fixture.colors, ", ") .. ".")

local members = api("GET", "/api/members")

local function member_id(username)
    for _, member in ipairs(members) do
        if member.username == username then
            return member.id
        end
    end
    if #members == 1 then
        return members[1].id
    end
    return nil
end

local task_count = #fixture.tasks
local log_date = format_date(capture_days - 1 + shift_days)
local done_name = columns[#columns].name
local done_position = 0

local function seed_task(i, task)
    local name = task.name
    local column = find_by_name(columns, task.column)
    if not column then
        verb.die(string.format("Task '%s' names the unknown column '%s'.", name, task.column))
    end
    local color_id = color_map[task.color]
    if not color_id then
        verb.die(string.format("Task '%s' has a color outside the palette.", name))
    end

    local create_column = column.id
    if task.column == done_name then
        create_column = first_column
    end

    local body = { name = name, column_id = create_column, color_id = color_id, force = true }
    if truthy(task.estimate_hours) then
        body.estimate_hours = task.estimate_hours
    end
    if truthy(task.due) then
        body.due_at = shift_due(task.due)
    end
    local task_id = api("POST", "/api/tasks", body).id

    local patch = { labels = truthy(task.labels) and task.labels or {} }
    if truthy(task.description) then
        patch.description = task.description
    end
    if truthy(task.assignee) then
        local assignee_id = member_id(task.assignee)
        if not assignee_id then
            verb.die(string.format("Task '%s' has an assignee with no matching user.", name))
        end
        patch.member_ids = { assignee_id }
    end
    if truthy(task.due_target_column) then
        local target = find_by_name(columns, task.due_target_column)
        if not target then
            verb.die(
                string.format(
                    "Task '%s' names the unknown due target column '%s'.",
                    name,
                    task.due_target_column
                )
            )
        end
        patch.due_target_column_id = target.id
    end
    api("PATCH", "/api/tasks/" .. task_id, patch)

    for _, subtask in ipairs(task.subtasks or {}) do
        local sub_id = api(
            "POST",
            "/api/tasks/" .. task_id .. "/subtasks",
            { name = subtask.name }
        ).id
        if truthy(subtask.done) then
            api("PATCH", "/api/tasks/" .. task_id .. "/subtasks/" .. sub_id, { done = true })
        end
    end

    for _, comment in ipairs(task.comments or {}) do
        api("POST", "/api/tasks/" .. task_id .. "/comments", { body = comment })
    end

    local hours = truthy(task.logged_hours) and task.logged_hours or 0
    local minutes = math.floor(hours * 60 + 0.5)
    if minutes > 0 then
        if minutes > MAX_LOGGED_MINUTES then
            verb.die(string.format("Task '%s' has logged_hours that do not fit in one day.", name))
        end
        api("POST", "/api/time/manual", {
            task_id = task_id,
            date = log_date,
            from = string.format("%02d:00", LOG_START_HOUR),
            to = string.format(
                "%02d:%02d",
                LOG_START_HOUR + math.floor(minutes / 60),
                minutes % 60
            ),
        })
    end

    if create_column ~= column.id then
        api(
            "POST",
            "/api/tasks/" .. task_id .. "/move",
            { column_id = column.id, position = done_position }
        )
        done_position = done_position + 1
    end
    log(string.format("Seeded task %d of %d, %s.", i, task_count, name))
end

for i, task in ipairs(fixture.tasks) do
    seed_task(i, task)
end

log("Fixture fields the API cannot reproduce exactly:")
log("  kanbanflow_board_id, kanbanflow_url: KanbanFlow metadata, no Pomb field")
log("  assignee: only registered users can be assigned; names map to the single admin")
log("  board members (header avatars): no API, derived from registered users")
log("  comments: author and timestamp are not in the fixture; posted now as the session user")
log(
    string.format(
        "  logged_hours: the fixture has no log date; entry placed at %s %02d:00",
        log_date,
        LOG_START_HOUR
    )
)
log("  Done column: completion time is the seed time, not the capture time")
io.stdout:write(board_id, "\n")
return 0
