local dev_command = require("@dev/command")
local devos = require("@dev/os")
local fs = require("@dev/fs")
local json = require("@dev/json")

local POLL_SECS = 0.2
local POLLS = 100

local function need(ok, err)
    if not ok then
        dev_command.die(err)
    end
end

return dev_command({
    summary = "Start a Pomb binary without sign-in under a frozen server clock and seed the parity boards",
    long = [[
Start a Pomb binary without sign-in (--auth none) under a frozen server
clock, seed the three parity boards and write WORKDIR/state.json for
run.mjs and regress.mjs.

BINARY is a release or debug pomb executable, PORT a free TCP port
(127.0.0.1 only), WORKDIR a scratch directory for the database, an empty
cookie jar, the log, the seeder stderr files and state.json.

The first request creates the owner, named "owner", and every cookie-less
request acts as that owner, who dismisses the first-visit hints through
the settings API so the screenshots show the board a returning user sees.
The Golden Master board gets one limit group over its second and third
columns, so a board's group band renders in every run.

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
]],
    remote = false,
    positional = true,
    args = {
        { name = "binary", required = true, help = "A release or debug pomb executable" },
        { name = "port", required = true, help = "A free TCP port, on 127.0.0.1 only" },
        { name = "workdir", required = true, help = "A scratch directory for the run's files" },
    },
    run = function(opts, args, ctx)
        -- Read the arguments
        local bin, port, work = args.binary, args.port, args.workdir

        -- Pick the fake clock times, zone and preload library
        local seed_time = devos.getenv("PARITY_SEED_TIME") or "2026-09-30 15:22:00"
        local serve_time = devos.getenv("PARITY_SERVE_TIME") or "2026-10-16 18:00:00"
        local tz = devos.getenv("PARITY_TZ") or "America/Chicago"
        local preload
        if devos.name() == "Linux" then
            local lib = devos.getenv("FAKETIME_LIB") or "/usr/lib/faketime/libfaketime.so.1"
            preload = { LD_PRELOAD = lib }
        else
            local lib = devos.getenv("FAKETIME_LIB")
                or "/opt/homebrew/lib/faketime/libfaketime.1.dylib"
            preload = { DYLD_INSERT_LIBRARIES = lib, DYLD_FORCE_FLAT_NAMESPACE = "1" }
        end
        local base = "http://127.0.0.1:" .. port

        -- Reset the work directory files
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

        -- Run curl and exit on failure
        local function curl(argv)
            local out, code = devos.capture(argv)
            if code ~= 0 then
                devos.exit(code)
            end
            return out
        end

        -- Send one JSON request to the server
        local function api(method, path, body)
            return curl({
                "curl",
                "-sS",
                "-f",
                "-b",
                jar,
                "-H",
                "Accept: application/json",
                "-H",
                "Content-Type: application/json",
                "-X",
                method,
                "--data-binary",
                body,
                base .. path,
            })
        end

        -- Tell whether the server answers
        local function up()
            local _, code = devos.capture({ "curl", "-s", "-o", "/dev/null", base .. "/login" })
            return code == 0
        end

        -- Start the server on a fake wall clock and wait for it
        local function start_server(wall)
            need(fs.write(ftfile, wall))
            local env = {
                TZ = tz,
                FAKETIME_TIMESTAMP_FILE = ftfile,
                FAKETIME_CACHE_DURATION = "0",
                FAKETIME_DONT_FAKE_MONOTONIC = "1",
            }
            for key, value in pairs(preload) do
                env[key] = value
            end
            local server = {
                bin,
                "--auth",
                "none",
                "--host",
                "127.0.0.1",
                "--port",
                port,
                "--database-path",
                db,
            }
            local proc = devos.spawn(server, {
                env = env,
                detach = true,
                stdin = "null",
                stdout = log,
                stderr = log,
            })
            if not proc then
                dev_command.note("The server did not come up.")
                devos.exit(1)
            end
            for _ = 1, POLLS do
                if up() then
                    return proc
                end
                devos.nap(POLL_SECS)
            end
            dev_command.note("The server did not come up.")
            devos.exit(1)
        end

        -- Kill the server and wait for it
        local function stop_server(proc)
            proc:kill()
            proc:wait(POLL_SECS * POLLS)
        end

        -- Run a seeder and return the last line it prints
        local function seed(argv, errfile)
            local out, code = devos.capture(argv, { stderr = errfile })
            if code ~= 0 then
                local text = fs.read(errfile) or ""
                local name = table.concat(argv, " ", 1, 3)
                dev_command.die(
                    "The seeder " .. name .. " failed with exit " .. code .. ". " .. text
                )
            end
            local last
            for line in out:gmatch("[^\n]+") do
                last = line
            end
            return last or ""
        end

        -- Start the server at the seed time
        local proc = start_server(seed_time)

        -- The regress job collects every seed-*.err in the work directory
        local gm_argv = { "dev", "tools", "seedgoldenmaster", "--as-of", "2026-09-30", base, jar }
        local gm = seed(gm_argv, work .. "/seed-gm.err")

        -- Configure the golden master board's columns and limits
        local columns_url = base .. "/api/boards/" .. gm .. "/columns"
        local columns = json.decode(curl({ "curl", "-sS", "-b", jar, columns_url }))
        for _, col in ipairs(columns) do
            api(
                "PATCH",
                "/api/columns/" .. col.id,
                '{"config_json":"{\\"prop_subtasks\\":\\"show\\"}"}'
            )
        end
        api("PUT", "/api/boards/" .. gm .. "/config", '{"legend_visible":true}')
        api(
            "POST",
            "/api/boards/" .. gm .. "/limit-groups",
            '{"column_ids":["' .. columns[2].id .. '","' .. columns[3].id .. '"],"limit":50}'
        )

        -- Restart at the serve time and seed the other boards
        stop_server(proc)
        proc = start_server(serve_time)
        for _, hint in ipairs({ "members", "timer" }) do
            api("PUT", "/api/settings", '{"dismiss_hint":"' .. hint .. '"}')
        end
        local vt = seed({ "dev", "parity", "seedvisualtest", base, jar }, work .. "/seed-vt.err")
        local ep = seed({ "dev", "parity", "seedepic", base, jar }, work .. "/seed-ep.err")
        local pn = seed({ "dev", "parity", "seedpanel", base, jar }, work .. "/seed-pn.err")

        -- Read the server's date header
        local date_header = ""
        for line in curl({ "curl", "-sI", base .. "/login" }):gmatch("[^\r\n]+") do
            if line:lower():match("^date:") then
                date_header = line
            end
        end

        -- Write and print the run state
        local state = json.encode({
            base = base,
            jar = jar,
            boards = { gm = gm, vt = vt, ep = ep, pn = pn },
            faketime = serve_time,
            ftfile = ftfile,
            tz = tz,
            pid = proc.pid,
            serverDate = date_header,
        }, {
            order = {
                "base",
                "jar",
                "boards",
                "faketime",
                "ftfile",
                "tz",
                "pid",
                "serverDate",
            },
        })
        need(fs.write(work .. "/state.json", state .. "\n"))
        dev_command.say(state)
        return 0
    end,
})
