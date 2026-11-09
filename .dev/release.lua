--[[
positional
flags: --batch --summary --env --dry-run --hash-only
Cut a release from the remote's main as it stands and print the homelab request.
Usage: dev release --batch N --summary "<text>" [--env KEY=VALUE]... [--dry-run]
       dev release --hash-only <sha> [--batch N] [--env KEY=VALUE]...
Each card lands on main through dev land after its own verification, so a release adds only the version bump. Steps, any failure aborts before the push:
1. Refuse unless <remote>/main has a commit since its last v* tag.
2. Bump the version with dev __bump_version in .dev/worktrees/release-<N> on <remote>/main and commit "Release batch-<N>: <summary>" with only Cargo.toml and Cargo.lock. Then refuse unless the new version differs from the last v* tag's and Cargo.lock's pomb entry matches it, and refuse when batch-<N> or v<version> already exists locally or on the remote.
3. Take the land lock .dev/release.lock in the main checkout, which names the batch and the pid and which dev land waits on, and drop it on every exit and once the push is done. Refuse when a live release holds it. Run dev lint --remote, dev test --remote, dev nixcheck --remote and dev build --remote through __gate, one at a time on ci, logging to a temp dir. A gate refused because the ci hub is draining is run again after POMB_DRAIN_INTERVAL seconds (default 60) until POMB_DRAIN_BOUND seconds (default 3600) have passed. Any other failing gate aborts before the push, and so does a gate whose ci run is not a new passed run of the release worktree with source local.
4. Refuse attribution trailers in the new commit and refuse when <remote>/main moved since step 1, then push to main on the remote of record.
5. Tag batch-<N> and v<version> at the pushed commit with __push_tag, then compute the nix hash of the commit's tree and print the request block. A failed tag prints the __push_tag command to rerun for it.
The remote of record is forge.remote, else gerrit when that remote exists, else origin. On gerrit the release commit carries a Change-Id trailer.
--hash-only <sha> fetches main from the remote of record and prints the request block for an already pushed sha: the recovery path when the hash step fails after the push.
--dry-run does everything except the push, the tags and the hash. The release worktree stays behind. dev verify --cleanup removes it once the release is live.
Repeat --env to list pomb.service environment changes in the request.
]]
local devos = require("dev.os")
local fs = require("dev.fs")
local forge = require("pomb.forge")
local lock = require("pomb.lock")

local ctx = ...

local QUIET = { stderr = "null" }

local lock_repo

local function say(...)
    io.stdout:write(...)
    io.stdout:flush()
end

local function die(message)
    if lock_repo then
        lock.drop(lock_repo)
    end
    io.stderr:write(message, "\n")
    os.exit(1)
end

local function usage()
    io.stderr:write('usage: dev release --batch N --summary "<text>" [--env KEY=VALUE]... [--dry-run]\n')
    io.stderr:write("       dev release --hash-only <sha> [--batch N] [--env KEY=VALUE]...\n")
    os.exit(2)
end

local function trim(text)
    return (text:gsub("%s+$", ""))
end

local function git(dir, ...)
    return { "git", "-C", dir, ... }
end

local function read_git(dir, ...)
    local out, code = devos.capture(git(dir, ...), QUIET)
    return trim(out), code
end

local function tag_taken(repo, remote, tag)
    local _, local_code = read_git(repo, "rev-parse", "-q", "--verify", "refs/tags/" .. tag)
    if local_code == 0 then
        return true
    end
    local listed, code = read_git(repo, "ls-remote", "--tags", remote, "refs/tags/" .. tag)
    if code ~= 0 then
        die(("Cannot list the tags of %s."):format(remote))
    end
    return listed ~= ""
end

local function parse(args)
    local found = { envs = {}, dry_run = false, batch = "", summary = "", hash_only = "" }
    local values = { ["--batch"] = "batch", ["--summary"] = "summary", ["--hash-only"] = "hash_only" }
    local i = 1
    while i <= #args do
        local arg = args[i]
        local name, inline = arg:match("^(%-%-[%w-]+)=(.*)$")
        name = name or arg
        if values[name] or name == "--env" then
            if not inline then
                i = i + 1
                inline = args[i] or ""
            end
            if name == "--env" then
                found.envs[#found.envs + 1] = inline
            else
                found[values[name]] = inline
            end
        elseif arg == "--dry-run" then
            found.dry_run = true
        else
            usage()
        end
        i = i + 1
    end
    if found.hash_only ~= "" then
        if found.summary ~= "" then
            usage()
        end
    elseif found.batch == "" or found.summary == "" then
        usage()
    end
    if found.batch:find("%D") then
        die(("The --batch value must be a number, got '%s'."):format(found.batch))
    end
    for _, kv in ipairs(found.envs) do
        if not kv:find("^[A-Za-z_][^=]*=") then
            die(("The --env value must be KEY=VALUE, got '%s'."):format(kv))
        end
    end
    return found
end

local function nix_hash(repo, sha)
    local dir = assert(fs.mktemp({ prefix = "release-hash", dir = true }))
    local out = ""
    local archive = devos.run({
        "bash",
        "-c",
        ("set -o pipefail; git -C %s archive %s | tar -x -C %s"):format(
            devos.quote(repo),
            devos.quote(sha),
            devos.quote(dir)
        ),
    })
    if archive == 0 then
        local hashed, code = devos.capture(
            { "nix", "--extra-experimental-features", "nix-command", "hash", "path", dir },
            QUIET
        )
        if code == 0 then
            out = trim(hashed)
        end
    end
    devos.run({ "rm", "-rf", dir })
    if out ~= "" then
        return out
    end
end

local function request_block(opts, sha, hash)
    say("\n===== homelab request =====\n")
    say(("Pomb sha: %s\n"):format(sha))
    say(("nix hash:     %s\n"):format(hash))
    if opts.batch ~= "" then
        say(("batch:        %s\n"):format(opts.batch))
    end
    if #opts.envs > 0 then
        say("pomb.service env changes:\n")
        for _, kv in ipairs(opts.envs) do
            say(("  %s\n"):format(kv))
        end
    end
    say("condition:    dry run must show only pomb.service\n")
    say("===========================\n")
end

local function main_repo_of(root)
    local out, code = read_git(root, "rev-parse", "--path-format=absolute", "--git-common-dir")
    if code ~= 0 then
        os.exit(1)
    end
    return (out:gsub("/%.git$", ""))
end

local opts = parse(ctx.args)
local repo = main_repo_of(ctx.root)

if opts.hash_only ~= "" then
    if not opts.hash_only:find("^[0-9a-f]+$") or #opts.hash_only ~= 40 then
        die("The --hash-only value must be a full 40 character sha.")
    end
    local remote, err = forge.remote(repo)
    if not remote then
        io.stderr:write(err, "\n")
        return 1
    end
    if devos.run(git(repo, "fetch", "-q", remote, "main")) ~= 0 then
        die("The fetch of main failed.")
    end
    local hash = nix_hash(repo, opts.hash_only)
    if not hash then
        die(("The nix hash failed for %s. Rerun: dev release --hash-only %s."):format(opts.hash_only, opts.hash_only))
    end
    request_block(opts, opts.hash_only, hash)
    return 0
end

local remote, rerr = forge.remote(repo)
if not remote then
    io.stderr:write(rerr, "\n")
    return 1
end
local fetched = devos.run(git(repo, "fetch", "-q", remote))
if fetched ~= 0 then
    return fetched or 1
end
local base = read_git(repo, "rev-parse", remote .. "/main")

local last_tag, code = read_git(repo, "describe", "--tags", "--match", "v*", "--abbrev=0", base)
if code ~= 0 then
    die(("No v* tag is in the history of %s/main."):format(remote))
end
if read_git(repo, "rev-list", "--count", last_tag .. ".." .. base) == "0" then
    die(("The %s/main (%s) has no commit since %s. Nothing to release."):format(remote, base:sub(1, 7), last_tag))
end

local rel = ".dev/worktrees/release-" .. opts.batch
local wt = repo .. "/" .. rel
if fs.exists(wt) then
    die(("The path %s already exists. Remove it first."):format(rel))
end
if devos.run(git(repo, "worktree", "add", "-q", "--detach", rel, base), { cwd = repo }) ~= 0 then
    die(("The git worktree add failed for %s."):format(rel))
end
local bumped
bumped, code = devos.capture({ "dev", "__bump_version", wt }, { cwd = wt })
if code ~= 0 then
    die(("The version bump failed. The worktree is left at %s."):format(wt))
end
local new_version = trim(bumped)
local message = ("Release batch-%s: %s"):format(opts.batch, opts.summary)
if
    devos.run(git(wt, "add", "Cargo.toml", "Cargo.lock")) ~= 0
    or devos.run(git(wt, "commit", "-q", "-m", message)) ~= 0
then
    die(("The commit of the version bump failed. The worktree is left at %s."):format(wt))
end
if remote == "gerrit" and not forge.add_change_id(wt) then
    die(("The Change-Id amend failed. The worktree is left at %s."):format(wt))
end
local sha = read_git(wt, "rev-parse", "HEAD")

local tag_toml = assert(fs.mktemp({ prefix = "release-tag" }))
local tagged
tagged, code = devos.capture(git(wt, "show", last_tag .. ":Cargo.toml"), QUIET)
if code ~= 0 then
    fs.remove(tag_toml)
    die(("Cannot read Cargo.toml at %s."):format(last_tag))
end
assert(fs.write(tag_toml, tagged))
local guard = devos.run({ "dev", "__version_guard", tag_toml, "Cargo.toml", "Cargo.lock" }, { cwd = wt })
fs.remove(tag_toml)
if guard ~= 0 then
    die(("The version %s fails the check against %s. The worktree is left at %s."):format(new_version, last_tag, wt))
end

for _, tag in ipairs({ "batch-" .. opts.batch, "v" .. new_version }) do
    if tag_taken(repo, remote, tag) then
        die(("Tag %s already exists locally or on %s. The worktree is left at %s."):format(tag, remote, wt))
    end
end

local logs = assert(fs.mktemp({ prefix = "release-" .. opts.batch, dir = true }))
say(("release: logs in %s\n"):format(logs))
local holder = lock.holder(repo)
if holder and lock.alive(holder) then
    die(("Release batch %s (pid %s) already holds the land lock. The worktree is left at %s."):format(
        holder.batch,
        holder.pid,
        wt
    ))
end
lock.take(repo, opts.batch)
lock_repo = repo
local DRAIN = "the hub is draining"
local DRAIN_INTERVAL_SECS = 60
local DRAIN_BOUND_SECS = 3600

local function positive_env(name, default)
    local value = tonumber(os.getenv(name))
    if value and value > 0 then
        return value
    end
    return default
end

local drain_interval = positive_env("POMB_DRAIN_INTERVAL", DRAIN_INTERVAL_SECS)
local drain_bound = positive_env("POMB_DRAIN_BOUND", DRAIN_BOUND_SECS)

local function run_gate(gate)
    return devos.capture({ "dev", "__gate", wt, gate, logs .. "/" .. gate .. ".log" }, {
        cwd = wt,
        env = { DEV_AGENT = "pomb/release/" .. opts.batch, CARGO_TARGET_DIR = wt .. "/target" },
        stderr = logs .. "/" .. gate .. ".err",
    })
end

local function drained(gate)
    return (fs.read(logs .. "/" .. gate .. ".log") or ""):find(DRAIN, 1, true) ~= nil
end

for _, gate in ipairs({ "lint", "test", "nixcheck", "build" }) do
    say(("release: dev %s --remote ...\n"):format(gate))
    local ran
    ran, code = run_gate(gate)
    local waited = 0
    while code ~= 0 and drained(gate) do
        if waited >= drain_bound then
            die(("The ci hub was still draining after %s seconds, so dev %s --remote did not run. See %s/%s.log. The worktree is left at %s."):format(
                drain_bound,
                gate,
                logs,
                gate,
                wt
            ))
        end
        say(
            ("release: dev %s --remote waits %s seconds because the ci hub is draining.\n"):format(
                gate,
                drain_interval
            )
        )
        devos.run({ "sleep", tostring(drain_interval) })
        waited = waited + drain_interval
        ran, code = run_gate(gate)
    end
    if code ~= 0 then
        local err = trim(fs.read(logs .. "/" .. gate .. ".err") or "")
        die(("The command dev %s --remote did not pass on this worktree: %s. See %s/%s.log. The worktree is left at %s."):format(
            gate,
            err,
            logs,
            gate,
            wt
        ))
    end
    say(("release: %s ci run %s\n"):format(gate, trim(ran)))
end
local summary = ""
for line in (fs.read(logs .. "/test.log") or ""):gmatch("[^\n]+") do
    summary = line:match("Summary.*") or summary
end
say(("release: gates ok: %s\n"):format(summary))

if devos.run({ "dev", "__push_main", wt, "--check" }, { cwd = wt }) ~= 0 then
    die(("The attribution check failed. The worktree is left at %s."):format(wt))
end
if devos.run(git(repo, "fetch", "-q", remote)) ~= 0 then
    die(("The fetch of %s failed."):format(remote))
end
if read_git(repo, "rev-parse", remote .. "/main") ~= base then
    die(("The %s/main branch moved past %s while the gates ran. Rerun dev release. The worktree is left at %s."):format(
        remote,
        base:sub(1, 7),
        wt
    ))
end
if opts.dry_run then
    lock.drop(repo)
    say(("release: dry run, not pushed. Release commit %s in %s\n"):format(sha, wt))
    return 0
end
if devos.run({ "dev", "__push_main", wt }, { cwd = wt }) ~= 0 then
    die(("The push failed. The worktree is left at %s."):format(wt))
end
lock.drop(repo)

local reruns = {}
for _, tag in ipairs({ "batch-" .. opts.batch, "v" .. new_version }) do
    if devos.run({ "dev", "__push_tag", tag, sha }, { cwd = wt }) ~= 0 then
        reruns[#reruns + 1] = ("dev __push_tag %s %s"):format(tag, sha)
    end
end
if #reruns > 0 then
    die(("Pushed %s but tagging failed. Rerun: %s. Then run: dev release --hash-only %s."):format(
        sha,
        table.concat(reruns, " && "),
        sha
    ))
end
local hash = nix_hash(repo, sha)
if not hash then
    die(("Pushed %s but the nix hash failed. Run: dev release --hash-only %s."):format(sha, sha))
end
request_block(opts, sha, hash)
return 0
