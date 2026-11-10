--[[
positional
Start a Pomb binary without sign-in (--auth none) under a frozen server
clock, seed the three parity boards and write WORKDIR/state.json for
run.mjs and regress.mjs.
Usage: dev parity start-server BINARY PORT WORKDIR
BINARY is a release or debug pomb executable, PORT a free TCP port
(127.0.0.1 only), WORKDIR a scratch directory for the database, an empty
cookie jar, the log, the seeder stderr files and state.json.
The first request creates the owner, named "owner", and every cookie-less
request acts as that owner, who dismisses the first-visit hints through
the settings API so the screenshots show the board a returning user sees.
The Golden Master board is seeded with the clock frozen at
PARITY_SEED_TIME (the fixture's capture day). The server then restarts on
the same database with the clock frozen at PARITY_SERVE_TIME (the day the
goldens were captured), so Done groups and overdue counts match the
goldens, and the Visual Test and Epic boards are seeded on that day.
Env: PARITY_SEED_TIME (default "2026-09-30 15:22:00"), PARITY_SERVE_TIME
(default "2026-10-16 18:00:00"), PARITY_TZ (default America/Chicago),
FAKETIME_LIB. Needs libfaketime (brew install libfaketime on macOS, the
faketime package on Linux). Stop the server with
`kill $(jq -r .pid WORKDIR/state.json)`.
]]
local devos = require("dev.os")
local fs = require("dev.fs")
local json = require("dev.json")
local verb = require("dev.verb")

local ctx = ...
local bin, port, work = ctx.args[1], ctx.args[2], ctx.args[3]
if not (bin and port and work) then
    local usage = "Binary, port and workdir are required."
    verb.die(usage .. " Usage: dev " .. ctx.name .. " BINARY PORT WORKDIR.")
end

local POLL_SECS = 0.2
local POLLS = 100

local seed_time = os.getenv("PARITY_SEED_TIME") or "2026-09-30 15:22:00"
local serve_time = os.getenv("PARITY_SERVE_TIME") or "2026-10-16 18:00:00"
local tz = os.getenv("PARITY_TZ") or "America/Chicago"
local preload
if devos.name() == "Linux" then
    local lib = os.getenv("FAKETIME_LIB") or "/usr/lib/faketime/libfaketime.so.1"
    preload = { "LD_PRELOAD=" .. lib }
else
    local lib = os.getenv("FAKETIME_LIB") or "/opt/homebrew/lib/faketime/libfaketime.1.dylib"
    preload = { "DYLD_INSERT_LIBRARIES=" .. lib, "DYLD_FORCE_FLAT_NAMESPACE=1" }
end
local base = "http://127.0.0.1:" .. port

local function need(ok, err)
    if not ok then
        verb.die(err)
    end
end

need(fs.mkdir(work, { parents = true }))
local db = work .. "/parity-" .. port .. ".redb"
local jar = work .. "/jar-" .. port .. ".txt"
local ftfile = work .. "/ft-" .. port .. ".txt"
local log = work .. "/server-" .. port .. ".log"
need(fs.remove(db, { recursive = true }))
need(fs.write(jar, ""))
need(fs.write(log, ""))
for _, name in ipairs({ "gm", "vt", "ep", "pn" }) do
    need(fs.write(work .. "/seed-" .. name .. ".err", ""))
end

local function curl(argv)
    local out, code = devos.capture(argv)
    if code ~= 0 then
        os.exit(code)
    end
    return out
end

local function api(method, path, body)
    return curl({
        "curl", "-sS", "-f", "-b", jar,
        "-H", "Accept: application/json", "-H", "Content-Type: application/json",
        "-X", method, "--data-binary", body, base .. path,
    })
end

local function up()
    local _, code = devos.capture({ "curl", "-s", "-o", "/dev/null", base .. "/login" })
    return code == 0
end

local LAUNCH = 'nohup "$@" >>"$LOG" 2>&1 </dev/null & echo $!'

local function start_server(wall)
    need(fs.write(ftfile, wall))
    local argv = {
        "sh", "-c", LAUNCH, "sh", "env",
        "TZ=" .. tz,
        "FAKETIME_TIMESTAMP_FILE=" .. ftfile,
        "FAKETIME_CACHE_DURATION=0",
        "FAKETIME_DONT_FAKE_MONOTONIC=1",
    }
    for _, word in ipairs(preload) do
        argv[#argv + 1] = word
    end
    local server = { bin, "--auth", "none", "--host", "127.0.0.1", "--port", port }
    server[#server + 1] = "--database-path"
    server[#server + 1] = db
    for _, word in ipairs(server) do
        argv[#argv + 1] = word
    end
    local out = devos.capture(argv, { env = { LOG = log } })
    local pid = tonumber(out:match("%d+"))
    for _ = 1, POLLS do
        if up() then
            return pid
        end
        devos.nap(POLL_SECS)
    end
    io.stderr:write("server did not start\n")
    os.exit(1)
end

local function stop_server(pid)
    devos.run({ "kill", tostring(pid) })
    for _ = 1, POLLS do
        if devos.run({ "kill", "-0", tostring(pid) }, { stderr = "null" }) ~= 0 then
            return
        end
        devos.nap(POLL_SECS)
    end
end

local function seed(argv, errfile)
    local out, code = devos.capture(argv, { stderr = errfile })
    if code ~= 0 then
        local text = fs.read(errfile) or ""
        local name = table.concat(argv, " ", 1, 3)
        verb.die("The seeder " .. name .. " failed with exit " .. code .. ". " .. text)
    end
    local last
    for line in out:gmatch("[^\n]+") do
        last = line
    end
    return last or ""
end

local pid = start_server(seed_time)

local gm_argv = { "dev", "tools", "seed-golden-master", "--as-of", "2026-09-30", base, jar }
local gm = seed(gm_argv, work .. "/seed-gm.err")

local columns_url = base .. "/api/boards/" .. gm .. "/columns"
local columns = json.decode(curl({ "curl", "-sS", "-b", jar, columns_url }))
for _, col in ipairs(columns) do
    api("PATCH", "/api/columns/" .. col.id, '{"config_json":"{\\"prop_subtasks\\":\\"show\\"}"}')
end
api("PUT", "/api/boards/" .. gm .. "/config", '{"legend_visible":true}')

stop_server(pid)
pid = start_server(serve_time)
for _, hint in ipairs({ "members", "timer" }) do
    api("PUT", "/api/settings", '{"dismiss_hint":"' .. hint .. '"}')
end
local vt = seed({ "dev", "parity", "seed-visual-test", base, jar }, work .. "/seed-vt.err")
local ep = seed({ "dev", "parity", "seed-epic", base, jar }, work .. "/seed-ep.err")
local pn = seed({ "dev", "parity", "seed-panel", base, jar }, work .. "/seed-pn.err")

local date_header = ""
for line in curl({ "curl", "-sI", base .. "/login" }):gmatch("[^\r\n]+") do
    if line:lower():match("^date:") then
        date_header = line
    end
end

local state = json.encode({
    base = base,
    jar = jar,
    boards = { gm = gm, vt = vt, ep = ep, pn = pn },
    faketime = serve_time,
    ftfile = ftfile,
    tz = tz,
    pid = pid,
    serverDate = date_header,
}, { order = { "base", "jar", "boards", "faketime", "ftfile", "tz", "pid", "serverDate" } })
need(fs.write(work .. "/state.json", state .. "\n"))
io.write(state, "\n")
return 0
