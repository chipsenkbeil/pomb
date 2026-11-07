--[[
positional
flags: --card
Apply a verified commit onto the remote's main and push it once lint and test pass on ci.
Usage: dev land <verified-commit> --card ID
Steps, any failure aborts before the push:
1. Fetch the remote of record. Refuse a commit that is not exactly one commit past its merge-base with <remote>/main, or that changes the package version in Cargo.toml or the pomb entry of Cargo.lock: only dev release bumps the version.
2. Apply the commit onto <remote>/main in .dev/worktrees/land-<ID>. On gerrit the commit gets a Change-Id trailer when it has none.
3. Refuse attribution trailers through __push_main --check, before any gate.
4. Run dev lint --remote, then dev test --remote, through __gate, one at a time on ci, logging to a temp dir. The first failing gate aborts.
5. Push to main through __push_main, remove the land worktree, print the landed sha, note it on the card with dev board note and move the card to Release with dev board move. Then remove the card's worktree under .dev/worktrees, whose HEAD is the verified commit, and its branch. In a jj repo (.jj exists) it also forgets the jj workspace at that path and deletes the bookmark of the same name. A worktree with uncommitted changes is kept, and the verb says why.
6. A push refused because main moved on (exit 3) removes the worktree, fetches, and repeats steps 2 to 5 on the new main, 3 attempts in all. It never forces the push.
The remote of record is forge.remote, else gerrit when that remote exists, else origin. A failed land leaves its worktree behind; remove it before landing the card again. A push refused a third time exits 1 with the worktree left behind, and so does any other push failure, which is not retried.
]]
local devos = require("dev.os")
local fs = require("dev.fs")
local forge = require("pomb.forge")
local verb = require("dev.verb")

local ctx = ...

local ATTEMPTS = 3
local QUIET = { stderr = "null" }

local function say(...)
    io.stdout:write(...)
    io.stdout:flush()
end

local function warn(...)
    io.stderr:write(...)
end

local function usage()
    warn("Pass a verified commit and --card ID, as in dev land COMMIT --card ID.\n")
    os.exit(2)
end

local function parse(args)
    local commit, card
    local i = 1
    while i <= #args do
        local arg = args[i]
        if arg == "--card" then
            if i == #args then
                usage()
            end
            i = i + 1
            card = args[i]
        elseif arg:sub(1, 7) == "--card=" then
            card = arg:sub(8)
        elseif arg:sub(1, 1) == "-" then
            usage()
        else
            if commit then
                usage()
            end
            commit = arg
        end
        i = i + 1
    end
    if not commit or not card or commit == "" or card == "" then
        usage()
    end
    if card:find("[^A-Za-z0-9]") then
        verb.die(("The --card value must be letters and digits, got '%s'."):format(card))
    end
    return commit, card
end

local function trim(text)
    return (text:gsub("%s+$", ""))
end

local function lines(text)
    local found = {}
    for line in text:gmatch("[^\n]+") do
        found[#found + 1] = line
    end
    return found
end

local function git(dir, ...)
    return { "git", "-C", dir, ... }
end

local function run_git(dir, ...)
    return devos.run(git(dir, ...))
end

local function read_git(dir, ...)
    local out, code = devos.capture(git(dir, ...), QUIET)
    return trim(out), code
end

local function manifest_version(text)
    local section
    for line in text:gmatch("[^\n]+") do
        if line:sub(1, 1) == "[" then
            section = line
        end
        local value = line:match('^version = "(.*)"$')
        if section == "[package]" and value then
            return value
        end
    end
    return ""
end

local function lock_version(text)
    local found = false
    for line in text:gmatch("[^\n]+") do
        local value = line:match('^version = "(.*)"$')
        if found and value then
            return value
        end
        found = found or line == 'name = "pomb"'
    end
    return ""
end

local function versions(dir, rev)
    local manifest = devos.capture(git(dir, "show", rev .. ":Cargo.toml"), QUIET)
    local lock = devos.capture(git(dir, "show", rev .. ":Cargo.lock"), QUIET)
    return manifest_version(manifest) .. "\n" .. lock_version(lock)
end

local function main_repo_of(root)
    local out, code = read_git(root, "rev-parse", "--path-format=absolute", "--git-common-dir")
    if code ~= 0 then
        os.exit(1)
    end
    return (out:gsub("/%.git$", ""))
end

local function forget_jj_workspace(repo, path)
    local listing, code = devos.capture(
        { "jj", "--no-pager", "workspace", "list", "-T", 'name ++ " " ++ root ++ "\\n"' },
        { cwd = repo }
    )
    if code ~= 0 then
        warn(("Could not list jj workspaces, so the workspace at %s is left in jj.\n"):format(path))
        return
    end
    for _, line in ipairs(lines(listing)) do
        local name, ws_root = line:match("^(%S+) (.*)$")
        if ws_root == path then
            local forget = { "jj", "--no-pager", "workspace", "forget", name }
            if devos.run(forget, { cwd = repo }) ~= 0 then
                warn(("Could not forget jj workspace %s.\n"):format(name))
            end
        end
    end
end

local function delete_jj_bookmark(repo, branch)
    local listing, code = devos.capture(
        { "jj", "--no-pager", "bookmark", "list", "-T", 'name ++ "\\n"' },
        { cwd = repo }
    )
    if code ~= 0 then
        warn(("Could not list jj bookmarks, so the bookmark %s is left in jj.\n"):format(branch))
        return
    end
    for _, name in ipairs(lines(listing)) do
        if name == branch then
            local delete = { "jj", "--no-pager", "bookmark", "delete", branch }
            if devos.run(delete, { cwd = repo }) ~= 0 then
                warn(("Could not delete jj bookmark %s.\n"):format(branch))
            end
            return
        end
    end
end

local function retire_if_landed(repo, cand, entry)
    local path, head, branch = entry.path, entry.head, entry.branch
    if not path or not path:find("/.dev/worktrees/", 1, true) then
        return
    end
    if head ~= cand or not branch then
        return
    end
    local dirty = read_git(path, "status", "--porcelain")
    if dirty ~= "" then
        say(("Kept %s and branch %s because it has uncommitted changes.\n"):format(path, branch))
        return
    end
    if run_git(repo, "worktree", "remove", path) ~= 0 then
        say(
            ("Kept %s and branch %s because git worktree remove refused it.\n"):format(path, branch)
        )
        return
    end
    if fs.is_dir(repo .. "/.jj") then
        forget_jj_workspace(repo, path)
        delete_jj_bookmark(repo, branch)
    end
    if run_git(repo, "show-ref", "--verify", "-q", "refs/heads/" .. branch) == 0 then
        run_git(repo, "branch", "-q", "-D", branch)
    end
    say(("Removed %s and deleted branch %s.\n"):format(path, branch))
end

local function retire_landed_worktrees(repo, cand)
    local listing = devos.capture(git(repo, "worktree", "list", "--porcelain"))
    local entry = {}
    for line in (listing:gsub("\n$", "") .. "\n\n"):gmatch("([^\n]*)\n") do
        if line:sub(1, 9) == "worktree " then
            entry.path = line:sub(10)
        elseif line:sub(1, 5) == "HEAD " then
            entry.head = line:sub(6)
        elseif line:sub(1, 18) == "branch refs/heads/" then
            entry.branch = line:sub(19)
        elseif line == "" then
            retire_if_landed(repo, cand, entry)
            entry = {}
        end
    end
end

local function apply(land)
    local repo, wt, remote, cand, base = land.repo, land.wt, land.remote, land.cand, land.base
    local added = run_git(repo, "worktree", "add", "-q", "--detach", wt, base)
    if added ~= 0 then
        os.exit(added or 1)
    end
    if land.fork == base then
        if run_git(wt, "reset", "-q", "--hard", cand) ~= 0 then
            verb.die(("Commit %s does not reset onto %s/main, and the worktree is left at %s."):format(
                land.commit,
                remote,
                wt
            ))
        end
    elseif devos.run(git(wt, "cherry-pick", cand), { stdout = "null" }) ~= 0 then
        run_git(wt, "cherry-pick", "--abort")
        verb.die(
            ("Commit %s does not apply onto %s/main, and the worktree is left at %s."):format(
                land.commit,
                remote,
                wt
            )
        )
    end
    if remote == "gerrit" then
        local body = devos.capture(git(wt, "log", "-1", "--format=%B", "HEAD"), QUIET)
        local trailers = devos.capture_input(git(wt, "interpret-trailers", "--parse"), body, QUIET)
        if not trailers:find("Change-Id:", 1, true) then
            if not forge.add_change_id(wt) then
                verb.die(("Could not add a Change-Id, and the worktree is left at %s."):format(wt))
            end
        end
    end
    return (read_git(wt, "rev-parse", "HEAD"))
end

local function gates(wt, card, attempt)
    if devos.run({ "dev", "__push_main", wt, "--check" }, { cwd = wt }) ~= 0 then
        verb.die(("The attribution check failed, and the worktree is left at %s."):format(wt))
    end
    local logs = assert(fs.mktemp({ prefix = "land-" .. card, dir = true }))
    say(("Attempt %s of %s, with logs in %s.\n"):format(attempt, ATTEMPTS, logs))
    for _, gate in ipairs({ "lint", "test" }) do
        say(("Running dev %s --remote.\n"):format(gate))
        local ran, code = devos.capture({ "dev", "__gate", wt, gate, logs .. "/" .. gate .. ".log" }, {
            cwd = wt,
            env = { DEV_AGENT = "pomb/land/" .. card },
            stderr = logs .. "/" .. gate .. ".err",
        })
        if code ~= 0 then
            local err = trim(fs.read(logs .. "/" .. gate .. ".err") or "")
            verb.die(
                ("The dev %s --remote gate did not pass on this worktree: %s, so see %s/%s.log, and the worktree is left at %s."):format(
                    gate,
                    err,
                    logs,
                    gate,
                    wt
                )
            )
        end
        say(("The %s gate ran as ci run %s.\n"):format(gate, trim(ran)))
    end
end

local commit, card = parse(ctx.args)
local repo = main_repo_of(ctx.root)
local remote, rerr = forge.remote(repo)
if not remote then
    warn(rerr, "\n")
    return 1
end
local fetched = run_git(repo, "fetch", "-q", remote)
if fetched ~= 0 then
    return fetched or 1
end
local cand, code = read_git(repo, "rev-parse", "--verify", commit .. "^{commit}")
if code ~= 0 then
    verb.die(("No such commit: %s."):format(commit))
end
local base
base, code = read_git(repo, "rev-parse", remote .. "/main")
if code ~= 0 then
    return code or 1
end
local fork
fork, code = read_git(repo, "merge-base", base, cand)
if code ~= 0 then
    verb.die(("Commit %s shares no history with %s/main."):format(commit, remote))
end
local count = read_git(repo, "rev-list", "--count", fork .. ".." .. cand)
if count ~= "1" then
    verb.die(
        ("Commit %s is %s commits past %s/main, expected exactly one."):format(commit, count, remote)
    )
end
if versions(repo, cand .. "^") ~= versions(repo, cand) then
    verb.die(
        ("Commit %s changes the package version in Cargo.toml or Cargo.lock, and only dev release bumps it."):format(
            commit
        )
    )
end

local wt = repo .. "/.dev/worktrees/land-" .. card
if fs.exists(wt) then
    verb.die(("Worktree .dev/worktrees/land-%s already exists, so remove it first."):format(card))
end
local sha
local attempt = 1
while true do
    base, code = read_git(repo, "rev-parse", remote .. "/main")
    if code ~= 0 then
        return code or 1
    end
    sha = apply({
        repo = repo,
        wt = wt,
        remote = remote,
        commit = commit,
        cand = cand,
        fork = fork,
        base = base,
    })
    gates(wt, card, attempt)
    local status = devos.run({ "dev", "__push_main", wt }, { cwd = wt })
    if status == 0 then
        break
    end
    if status ~= 3 then
        verb.die(("The push failed with status %s, and the worktree is left at %s."):format(status, wt))
    end
    if attempt >= ATTEMPTS then
        verb.die(
            ("The branch %s/main moved on %s times, so nothing was pushed, the worktree is left at %s, and you should land the card again."):format(
                remote,
                ATTEMPTS,
                wt
            )
        )
    end
    say(("The branch %s/main moved on, so the commit is applied again.\n"):format(remote))
    run_git(repo, "worktree", "remove", "--force", wt)
    run_git(repo, "fetch", "-q", remote)
    attempt = attempt + 1
end

run_git(repo, "worktree", "remove", "--force", wt)
say(("Landed %s.\n"):format(sha))
local note = { "dev", "board", "note", card, ("landed %s on %s/main"):format(sha, remote) }
if devos.run(note, { cwd = repo }) ~= 0 then
    warn(
        ("Landed, but dev board note %s failed, so note %s on the card yourself.\n"):format(card, sha)
    )
end
if devos.run({ "dev", "board", "move", card, "release" }, { cwd = repo }) ~= 0 then
    warn(
        ("Landed, but dev board move %s release failed, so move the card yourself.\n"):format(card)
    )
end
retire_landed_worktrees(repo, cand)
return 0
