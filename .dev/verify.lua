--[[
positional
flags: --backup --cleanup --delete-branch
Check a release on green-box, and clean up its worktrees.
Usage: dev verify SHA [--backup FILE] [--cleanup WORKTREE...] [--delete-branch BRANCH]...
Read-only checks against the live service over ssh, each printed as PASS or FAIL: build_sha equals the release sha, prod app.js equals the release tree's, a cross-site POST gets 403, the journal shows the schema and listening lines, public /login answers as an install should, OpenAPI servers[0].url is the public URL, and a bad Bearer token gets code token_invalid. --backup takes a file name or a path under /var/lib/pomb-backups/, and also checks that the named database copy exists on the host. Any FAIL exits non-zero and nothing else happens.
--cleanup removes the named .dev/worktrees/* and deletes their branches when fully merged into main on the remote of record, and --delete-branch forces named branches too.
]]
local backup = require("pomb.backup")
local devos = require("dev.os")
local forge = require("pomb.forge")
local fs = require("dev.fs")
local json = require("dev.json")
local verb = require("dev.verb")

local ctx = ...
local stderr = io.stderr

local HOST = "green-box"
local PUBLIC_URL = "https://pomb.chip.network"
local WORKTREES = ".dev/worktrees/"
local SHORT = 12
local USAGE = "Usage: dev verify takes a release sha, then optional --backup, --cleanup"
    .. " and --delete-branch arguments."
local USAGE_CODE = 2
local QUIET = { stderr = "null" }

local function abort(message)
    stderr:write(message, "\n")
    os.exit(1)
end

local function usage()
    stderr:write(USAGE, "\n")
    os.exit(USAGE_CODE)
end

local function parse(args)
    local parsed = { cleanup = {}, forced = {} }
    local mode
    local i = 1
    while i <= #args do
        local arg = args[i]
        if arg == "--backup" then
            local value = args[i + 1]
            if not value or value == "" or value:sub(1, 1) == "-" then
                usage()
            end
            parsed.backup = value
            i = i + 1
            mode = nil
        elseif arg == "--cleanup" then
            mode = "cleanup"
        elseif arg == "--delete-branch" then
            parsed.forced[args[i + 1] or ""] = true
            i = i + 1
            mode = nil
        elseif arg:sub(1, 1) == "-" then
            usage()
        elseif mode == "cleanup" then
            parsed.cleanup[#parsed.cleanup + 1] = arg
        else
            if parsed.sha then
                usage()
            end
            parsed.sha = arg
        end
        i = i + 1
    end
    if not parsed.sha then
        usage()
    end
    return parsed
end

local function trim(text)
    return (text:gsub("^%s+", ""):gsub("%s+$", ""))
end

local function git(repo, argv)
    local words = { "git", "-C", repo }
    for _, word in ipairs(argv) do
        words[#words + 1] = word
    end
    return words
end

local function remote(command)
    local out = devos.capture({ "ssh", HOST, command })
    return out
end

local function sha256(data)
    local out = devos.capture_input({ "shasum", "-a", "256" }, data, QUIET)
    return out:match("^(%x+)") or ""
end

local function decode(text)
    local ok, doc = pcall(json.decode, text)
    if ok and type(doc) == "table" then
        return doc
    end
    return {}
end

local function text_of(value)
    if type(value) == "string" then
        return value
    end
    return ""
end

local function or_none(value)
    return value ~= "" and value or "none"
end

local function main_repo()
    local common = devos.capture({
        "git",
        "-C",
        ctx.root,
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    }, QUIET)
    return fs.dirname(trim(common))
end

local function login_case(public, location)
    if public == "200" then
        return true, "The public /login page answered 200, the passkey sign-in page."
    elseif public == "303" and location == "/" then
        return true, "The /login page redirected to /, with no sign-in, as --auth none allows."
    elseif public == "303" and location == "/setup" then
        return true, "The /login page redirected to /setup, as expected before the first passkey."
    end
    return false, ("The public /login page answered with status %s and location %s."):format(
        or_none(public),
        or_none(location)
    )
end

local function login_probe()
    local headers = devos.capture(
        { "curl", "-s", "-o", "/dev/null", "-D", "-", PUBLIC_URL .. "/login" },
        QUIET
    )
    local public, location = "", ""
    local first = true
    for line in headers:gsub("\r", ""):gmatch("[^\n]+") do
        if first then
            public = line:match("^%S+%s+(%S+)") or ""
            first = false
        elseif location == "" then
            location = line:match("^[Ll][Oo][Cc][Aa][Tt][Ii][Oo][Nn]:%s*(%S+)") or ""
        end
    end
    return login_case(public, location)
end

local function cleanup_worktree(worktree, opts)
    local repo, remote_name, forced = opts.repo, opts.remote_name, opts.forced
    worktree = worktree:gsub("/+$", "")
    local under = worktree:sub(1, #WORKTREES) == WORKTREES
        or worktree:sub(1, #repo + 1 + #WORKTREES) == repo .. "/" .. WORKTREES
    if not under then
        print(("Skipped %s, which is not under .dev/worktrees."):format(worktree))
        return
    end
    local path = worktree:sub(1, 1) == "/" and worktree or repo .. "/" .. worktree
    local head = devos.capture(git(path, { "symbolic-ref", "--short", "-q", "HEAD" }), QUIET)
    local branch = trim(head)
    local removal = git(repo, { "worktree", "remove", "--force", worktree })
    local reason, removed = devos.capture(removal, { stderr = "stdout" })
    if removed ~= 0 then
        local why = trim(reason):gsub("%s*\n%s*", " "):gsub("%.$", "")
        print(("Skipped %s, which could not be removed, and git said: %s."):format(worktree, why))
        return
    end
    print(("Removed %s."):format(worktree))
    if branch == "" then
        return
    end
    local ancestor = git(repo, { "merge-base", "--is-ancestor", branch, remote_name .. "/main" })
    if devos.run(ancestor) == 0 then
        if devos.run(git(repo, { "branch", "-q", "-d", branch })) == 0 then
            print(("Deleted branch %s, which was merged."):format(branch))
        end
    elseif forced[branch] then
        if devos.run(git(repo, { "branch", "-q", "-D", branch })) == 0 then
            print(("Deleted branch %s by force."):format(branch))
        end
    else
        print(("Kept branch %s, which is not merged, so pass --delete-branch %s to delete it."):format(branch, branch))
    end
end

local parsed = parse(ctx.args)
local backup_path
if parsed.backup then
    local why
    backup_path, why = backup.resolve(parsed.backup)
    if not backup_path then
        abort(why)
    end
end
local repo = main_repo()
local remote_name, why = forge.remote(repo)
if not remote_name then
    verb.note(("Dev verify cannot run, because %s."):format(why))
    return 1
end
if devos.run(git(repo, { "fetch", "-q", remote_name })) ~= 0 then
    return 1
end
local resolved, code = devos.capture(
    git(repo, { "rev-parse", "--verify", parsed.sha .. "^{commit}" }),
    QUIET
)
if code ~= 0 then
    abort(("No commit %s exists."):format(parsed.sha))
end
local sha = trim(resolved)

local failures = 0
local function check(ok, sentence)
    print(("%s %s"):format(ok and "PASS" or "FAIL", sentence))
    if not ok then
        failures = failures + 1
    end
end

local live = text_of(decode(remote("curl -s localhost:3000/api/v1/version")).build_sha)
check(
    live == sha,
    ("The live build sha is %s, and the release sha is %s."):format(or_none(live), sha)
)

local prod_js = remote("curl -s localhost:3000/static/app.js | shasum -a 256"):match("^(%S+)") or ""
local app, shown = devos.capture(git(repo, { "show", sha .. ":static/app.js" }))
if shown ~= 0 then
    return 1
end
local tree_js = sha256(app)
check(
    prod_js ~= "" and prod_js == tree_js,
    ("The prod app.js hash starts with %s, and the release app.js hash starts with %s."):format(
        or_none(prod_js:sub(1, SHORT)),
        or_none(tree_js:sub(1, SHORT))
    )
)

local xsite = trim(remote(
    'curl -s -o /dev/null -w "%{http_code}" -X POST -H "Origin: https://evil.example"'
        .. ' -H "Sec-Fetch-Site: cross-site" localhost:3000/api/boards'
))
check(
    xsite == "403",
    ("A cross-site POST got status %s, and 403 is expected."):format(or_none(xsite))
)

local journal = remote(
    'journalctl -u pomb --since "$(systemctl show pomb -p ActiveEnterTimestamp --value)"'
        .. ' --no-pager | grep -iE "schema|listening" | tail -2'
):gsub("\n+$", "")
if journal ~= "" then
    for line in (journal .. "\n"):gmatch("([^\n]*)\n") do
        print("      " .. line)
    end
end
local lowered = journal:lower()
local journal_ok = lowered:find("schema", 1, true) ~= nil
    and lowered:find("listening", 1, true) ~= nil
check(
    journal_ok,
    journal_ok and "The journal shows schema and listening lines since the last start."
        or "The journal lacks a schema or listening line since the last start."
)

local login_ok, login_detail = login_probe()
check(login_ok, login_detail)

local spec = decode(remote("curl -s localhost:3000/api/v1/openapi.json"))
local first_server = type(spec.servers) == "table" and spec.servers[1] or nil
local servers = type(first_server) == "table" and text_of(first_server.url) or ""
check(
    servers == PUBLIC_URL,
    ("The OpenAPI servers[0].url is %s, and the public URL is %s."):format(
        or_none(servers),
        PUBLIC_URL
    )
)

local token_code = text_of(
    decode(remote('curl -s -H "Authorization: Bearer pomb_nope" localhost:3000/api/boards')).code
)
check(
    token_code == "token_invalid",
    ("A bad Bearer token got code %s, and token_invalid is expected."):format(or_none(token_code))
)

if backup_path then
    local test = { "ssh", HOST, "sudo test -s " .. devos.quote(backup_path) }
    local present = devos.run(test, { stdout = "null", stderr = "null" }) == 0
    local found = present and "is present on" or "is missing from"
    check(present, ("The backup %s %s the host."):format(backup_path, found))
end

if failures > 0 then
    io.stdout:flush()
    stderr:write(("\n%d check(s) failed, so the worktrees are untouched.\n"):format(failures))
    return 1
end
print("\nAll checks passed.")

for _, worktree in ipairs(parsed.cleanup) do
    cleanup_worktree(worktree, { repo = repo, remote_name = remote_name, forced = parsed.forced })
end
return 0
