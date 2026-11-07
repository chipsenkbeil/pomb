--[[
local
positional
flags: --wait
Check a channel after an update: dev site verify [<stable|nightly> [<expected>] ] [--wait]
]]
local devos = require("dev.os")
local json = require("dev.json")
local site = require("pomb.site")

local API = "https://api.github.com/repos/chipsenkbeil/pomb"
local URLS = {
    [site.CHANNEL.STABLE] = "https://pomb.app",
    [site.CHANNEL.NIGHTLY] = "https://nightly.pomb.app",
}
local WAIT_FLAG = "--wait"
local USAGE = "usage: dev site verify [<stable|nightly> [<expected>]] [--wait]\n"
local WAIT_LIMIT_SECS = 1200
local WAIT_STEP_SECS = 15
local RELEASE_STEP_SECS = 60
local SSH_TRIES = 3
local FETCH_RETRIES = 3
local FETCH_TIMEOUT_SECS = 60
local VERSION_TIMEOUT_SECS = 10
local PROBE_TIMEOUT_SECS = 15
local RESOLVE = { OK = 0, RETRY = 1, INVALID = 2 }
local SSH_RETRY_SECS = 5
local SHORT_ID = 12
local SHA_MIN_LEN = 7
local SHA_MAX_LEN = 40
local REPORT = {
    LAST_UPDATE = "last-update ",
    JOURNAL_BAD = "journal-bad ",
    JOURNAL_LINES = "journal-lines ",
    SETUP_CODE = "setupcode ",
    RELEASES = "releases ",
    BACKUP = "backup ",
    ARCHIVES = "archives ",
}
local HEADER_CAPTURE =
    {
        "curl",
        "-sS",
        "--max-time",
        tostring(PROBE_TIMEOUT_SECS),
        "-o",
        "/dev/null",
        "-D",
        "-",
        "-w",
        "status=%{http_code}\n",
    }

local REMOTE_SCRIPT = [==[
set -u
unit="pomb-$CH.service"
invocation=$(systemctl show -p InvocationID --value "$unit")
echo "last-update $(cat "/var/lib/pomb/last-update-$CH" 2>/dev/null || echo never)"
if journalctl -u "$unit" --no-pager -q "_SYSTEMD_INVOCATION_ID=$invocation" | grep -q 'setup code'; then
  echo "setupcode present"
else
  echo "setupcode absent"
fi
echo "journal-lines $(journalctl -u "$unit" --no-pager -q "_SYSTEMD_INVOCATION_ID=$invocation" | wc -l)"
journalctl -u "$unit" --no-pager -q "_SYSTEMD_INVOCATION_ID=$invocation" | grep -Ei 'panic|error' | sed -E 's/setup code.*/setup code (hidden)/' | head -n 5 | sed 's/^/journal-bad /'
if find "/var/lib/pomb/$CH/backups" -maxdepth 1 -type f -name "pomb-*-before-$ID.redb" 2>/dev/null | grep -q .; then
  echo "backup present"
else
  echo "backup absent"
fi
if find "/var/lib/pomb/$CH/backups" -maxdepth 1 -type d -name "pomb-*-before-$ID.archives" 2>/dev/null | grep -q .; then
  echo "archives present"
else
  echo "archives absent"
fi
echo "releases $(find "/opt/pomb/$CH/releases" -mindepth 1 -maxdepth 1 -type d ! -name '.tmp-*' | wc -l)"
]==]

local function short(text)
    return text:sub(1, SHORT_ID)
end

local function lines_of(text)
    local lines = {}
    for line in (text .. "\n"):gmatch("(.-)\n") do
        lines[#lines + 1] = line
    end
    return lines
end

local function field(text, prefix)
    local found = {}
    for _, line in ipairs(lines_of(text)) do
        if line:sub(1, #prefix) == prefix then
            found[#found + 1] = line:sub(#prefix + 1)
        end
    end
    return found
end

local function first_field(text, prefix)
    return field(text, prefix)[1] or ""
end

local function header(text, name)
    for _, line in ipairs(lines_of(text)) do
        if line:lower():sub(1, #name + 2) == name .. ": " then
            return line:sub(#name + 3)
        end
    end
    return ""
end

local function fetch(url)
    local out, code = devos.capture({
        "curl",
        "-fsS",
        "--retry",
        tostring(FETCH_RETRIES),
        "--max-time",
        tostring(FETCH_TIMEOUT_SECS),
        "-H",
        "Accept: application/vnd.github+json",
        url,
    })
    return out, code == 0
end

local function decode_object(text)
    local value = json.decode(text)
    if type(value) == "table" then
        return value
    end
    return {}
end

local function string_field(object, key)
    local value = object[key]
    if type(value) == "string" then
        return value
    end
    return ""
end

local function release_commit(text)
    for _, line in ipairs(lines_of(string_field(decode_object(text), "body"))) do
        local hex = line:match("^%s*commit:%s*([0-9a-f]+)")
        if hex and #hex >= SHA_MIN_LEN then
            return hex:sub(1, SHA_MAX_LEN)
        end
    end
    return ""
end

local function is_sha(text)
    return #text >= SHA_MIN_LEN and #text <= SHA_MAX_LEN and text:find("^[0-9a-f]+$") ~= nil
end

local function resolve(channel, st)
    if channel == site.CHANNEL.NIGHTLY then
        if st.expected == "" then
            local rel, ok = fetch(API .. "/releases/tags/nightly")
            if not ok then
                return RESOLVE.RETRY
            end
            st.expected = release_commit(rel)
        end
        if not is_sha(st.expected) then
            return RESOLVE.INVALID
        end
        st.commit = st.expected
        st.id = short(st.commit)
        return RESOLVE.OK
    end
    local rel, ok
    if st.expected == "" then
        rel, ok = fetch(API .. "/releases/latest")
        if not ok then
            return RESOLVE.RETRY
        end
        st.expected = string_field(decode_object(rel), "tag_name")
    else
        rel, ok = fetch(API .. "/releases/tags/" .. st.expected)
        if not ok then
            return RESOLVE.RETRY
        end
    end
    if st.expected == "" or st.expected:find("/", 1, true) or st.expected:sub(1, 1) == "." then
        return RESOLVE.INVALID
    end
    st.id = st.expected
    st.commit = release_commit(rel)
    if st.commit == "" then
        local body, fetched = fetch(API .. "/commits/" .. st.id)
        st.commit = fetched and string_field(decode_object(body), "sha") or ""
    end
    if st.commit == "" then
        return RESOLVE.RETRY
    end
    return RESOLVE.OK
end

local function public_sha(url)
    local out = devos.capture(
        { "curl", "-fsS", "--max-time", tostring(VERSION_TIMEOUT_SECS), url .. "/api/v1/version" },
        { stderr = "null" }
    )
    return string_field(decode_object(out), "build_sha")
end

local function same_commit(commit, got)
    return got ~= "" and (commit:sub(1, #got) == got or got:sub(1, #commit) == commit)
end

local function probe(url, path)
    local argv = site.append({}, HEADER_CAPTURE)
    argv[#argv + 1] = url .. path
    local out = devos.capture(argv, { stderr = "null" })
    return (out:gsub("\r", ""))
end

local function remote_report(channel, id)
    local argv = site.ssh_argv(site.host(), { "CH=" .. channel, "ID=" .. id, "bash", "-s" })
    for _ = 1, SSH_TRIES do
        local out, code = devos.capture_input(argv, REMOTE_SCRIPT .. "\n", { stderr = "null" })
        if code == 0 then
            return out
        end
        devos.nap(SSH_RETRY_SECS)
    end
    return nil
end

local function result_matches(last, id)
    local at = 1
    while true do
        local from, to = last:find("ok: " .. id, at, true)
        if not from then
            break
        end
        local after = last:sub(to + 1, to + 1)
        if after == "" or after == " " then
            return true
        end
        at = from + 1
    end
    return last:find("failed: " .. id .. " ", 1, true) ~= nil
end

local function recorded_ok(last, id)
    local rest = last:match("^[0-9TZ:%-]+ ok: (.*)$")
    return rest ~= nil and (rest == id or rest:sub(1, #id + 1) == id .. " ")
end

local function verify(opts)
    local channel, expected, wait = opts.channel, opts.expected, opts.wait
    local url = URLS[channel]
    local failures = 0
    local function pass(text)
        io.write("PASS  ", text, "\n")
    end
    local function note(text)
        io.write("NOTE  ", text, "\n")
    end
    local function fail(text)
        io.write("FAIL  ", text, "\n")
        failures = failures + 1
    end

    site.host()
    local st = { expected = expected, commit = "", id = "" }
    local deadline = os.time() + WAIT_LIMIT_SECS
    while true do
        local rc = resolve(channel, st)
        if rc == RESOLVE.OK then
            break
        end
        if rc == RESOLVE.INVALID or not wait or os.time() >= deadline then
            fail(
                "cannot work out what "
                    .. channel
                    .. " should run"
                    .. (st.expected ~= "" and " (expected " .. st.expected .. ")" or "")
            )
            return 1
        end
        io.write(
            "verify: no ",
            channel,
            " release",
            st.expected ~= "" and " for " .. st.expected or "",
            " yet, waiting\n"
        )
        io.stdout:flush()
        devos.nap(RELEASE_STEP_SECS)
    end
    local commit, id = st.commit, st.id
    io.write("verify: ", channel, " expects ", id, " (commit ", short(commit), ")\n")

    deadline = os.time() + WAIT_LIMIT_SECS
    if wait then
        while true do
            local got = public_sha(url)
            if same_commit(commit, got) then
                break
            end
            if os.time() >= deadline then
                io.write("verify: gave up waiting after ", WAIT_LIMIT_SECS, " seconds\n")
                break
            end
            io.write(
                "verify: ",
                channel,
                " runs ",
                short(got),
                ", waiting for ",
                short(commit),
                "\n"
            )
            io.stdout:flush()
            devos.nap(WAIT_STEP_SECS)
        end
    end

    local got = public_sha(url)
    if same_commit(commit, got) then
        pass("build_sha at " .. url .. " is " .. short(got))
    else
        fail(
            "build_sha at "
                .. url
                .. " is "
                .. (got ~= "" and got or "unreachable")
                .. ", wanted "
                .. short(commit)
        )
    end

    local login = probe(url, "/login")
    local setup = probe(url, "/setup")
    local login_status = first_field(login, "status=")
    local setup_status = first_field(setup, "status=")
    if login_status == "200" and setup_status == "303" and header(setup, "location") == "/" then
        pass("/login answers 200 and /setup redirects to /, an instance with a passkey")
    elseif
        login_status == "303"
        and header(login, "location") == "/setup"
        and setup_status == "200"
    then
        pass("/login redirects to /setup and /setup answers 200, a fresh instance")
    else
        fail(
            "/login answered "
                .. (login_status ~= "" and login_status or "nothing")
                .. " and /setup "
                .. (setup_status ~= "" and setup_status or "nothing")
        )
    end

    local csp = header(login, "content-security-policy")
    local function has(text)
        return csp:find(text, 1, true) ~= nil
    end
    if
        has("default-src 'self'")
        and has("script-src 'self'")
        and has("frame-ancestors 'none'")
        and not has("unsafe-eval")
    then
        pass("content-security-policy is strict")
    else
        fail("content-security-policy is missing or loose")
    end
    local hsts = header(login, "strict-transport-security")
    if hsts:find("max%-age=[1-9]%d*") then
        pass("strict-transport-security is set (" .. hsts .. ")")
    else
        fail("strict-transport-security is missing")
    end

    local report
    while true do
        report = remote_report(channel, id)
        local last = first_field(report or "", REPORT.LAST_UPDATE)
        if
            not wait
            or report == nil
            or result_matches(last, id)
            or os.time() >= deadline
        then
            break
        end
        devos.nap(WAIT_STEP_SECS)
    end

    if report then
        local last = first_field(report, REPORT.LAST_UPDATE)
        if recorded_ok(last, id) then
            pass("last-update-" .. channel .. " records ok for " .. id)
        else
            fail("last-update-" .. channel .. " is: " .. last)
        end

        local bad = field(report, REPORT.JOURNAL_BAD)
        local lines = first_field(report, REPORT.JOURNAL_LINES)
        if #bad == 0 then
            pass(
                "journal since the unit started has no panic or error lines ("
                    .. lines
                    .. " lines)"
            )
        else
            fail("journal since the unit started has panic or error lines")
            for _, line in ipairs(bad) do
                io.write("        ", line, "\n")
            end
        end
        note("setup code in the journal: " .. first_field(report, REPORT.SETUP_CODE))

        local releases = first_field(report, REPORT.RELEASES)
        if first_field(report, REPORT.BACKUP) == "present" then
            pass("a pre-update backup named for " .. id .. " exists")
            if first_field(report, REPORT.ARCHIVES) == "present" then
                pass("its board archive directory exists")
            else
                note(
                    "no board archive directory beside the backup for "
                        .. id
                        .. " (the binary running before the update has no archive command before 0.5.30)"
                )
            end
        elseif releases == "1" then
            note(
                "no pre-update backup for "
                    .. id
                    .. ", and it is the only release on the box: a first install"
            )
        else
            fail("no pre-update backup named for " .. id)
        end
    else
        fail("cannot reach the box over ssh after " .. SSH_TRIES .. " tries")
    end

    if failures > 0 then
        io.write("verify: ", channel, " FAILED (", failures, ")\n")
        return 1
    end
    io.write("verify: ", channel, " ok\n")
    return 0
end

local function usage()
    io.stderr:write(USAGE)
    return site.USAGE
end

local ctx = ...
local wait = false
local args = {}
for _, word in ipairs(ctx.args) do
    if word == WAIT_FLAG then
        wait = true
    elseif word:sub(1, 1) == "-" then
        return usage()
    else
        args[#args + 1] = word
    end
end
if #args > 2 then
    return usage()
end

local channel = args[1] or ""
local expected = args[2] or ""
if channel == "" then
    if expected ~= "" then
        return usage()
    end
elseif not site.is_channel(channel) then
    return usage()
end

if channel == "" then
    local rc = 0
    for _, name in ipairs({ site.CHANNEL.STABLE, site.CHANNEL.NIGHTLY }) do
        io.write("== ", name, "\n")
        if verify({ channel = name, expected = "", wait = wait }) ~= 0 then
            rc = 1
        end
    end
    return rc
end
return verify({ channel = channel, expected = expected, wait = wait })
