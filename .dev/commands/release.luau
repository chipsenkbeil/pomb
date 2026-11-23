local dev_command = require("@dev/command")
local devos = require("@dev/os")
local fs = require("@dev/fs")
local forge = require("@lib/pomb/forge")
local lock = require("@lib/pomb/lock")

local QUIET = { stderr = "null" }
local GATES = { "lint", "test", "nixcheck", "build" }

local lock_repo

local function say(...)
    devos.stdout:write(...)
    devos.stdout:flush()
end

local function die(message)
    if lock_repo then
        lock.drop(lock_repo)
    end
    dev_command.note(message)
    devos.exit(1)
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

local function helper(ctx, name)
    return ctx.root .. "/.dev/helpers/" .. name .. ".luau"
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

local function parse(opts, ctx)
    local found = {
        envs = opts.env or {},
        dry_run = opts.dry_run or false,
        batch = opts.batch or "",
        summary = opts.summary or "",
        hash_only = opts.hash_only or "",
    }
    if found.hash_only ~= "" then
        if found.summary ~= "" then
            dev_command.usage_error(ctx, "The --hash-only option takes no --summary.")
        end
    elseif found.batch == "" or found.summary == "" then
        dev_command.usage_error(ctx, "Give --batch and --summary, or --hash-only.")
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
    fs.remove(dir, { recursive = true })
    if out ~= "" then
        return out
    end
end

local function request_block(found, sha, hash)
    say("\n===== homelab request =====\n")
    say(("Pomb sha: %s\n"):format(sha))
    say(("nix hash:     %s\n"):format(hash))
    if found.batch ~= "" then
        say(("batch:        %s\n"):format(found.batch))
    end
    if #found.envs > 0 then
        say("pomb.service env changes:\n")
        for _, kv in ipairs(found.envs) do
            say(("  %s\n"):format(kv))
        end
    end
    say("condition:    dry run must show only pomb.service\n")
    say("===========================\n")
end

local function main_repo_of(root)
    local out, code = read_git(root, "rev-parse", "--path-format=absolute", "--git-common-dir")
    if code ~= 0 then
        devos.exit(1)
    end
    return (out:gsub("/%.git$", ""))
end

return dev_command({
    summary = "Cut a release from the remote's main as it stands and print the homelab request.",
    remote = false,
    positional = true,
    requires = { "workspace" },
    flags = {
        { name = "batch", kind = "string", help = "the batch number" },
        { name = "summary", kind = "string", help = "the release commit's summary" },
        {
            name = "env",
            kind = "string",
            repeatable = true,
            help = "a pomb.service environment change as KEY=VALUE",
        },
        { name = "dry-run", kind = "boolean", help = "do everything except push, tag and hash" },
        {
            name = "hash-only",
            kind = "string",
            help = "print the request block for this pushed sha",
        },
    },
    long = [[
Each card lands on main through dev land after its own verification, so a release adds only the version bump. Steps, any failure aborts before the push:
1. Refuse unless <remote>/main has a commit since its last v* tag.
2. Bump the version with the bump_version helper in .dev/worktrees/release-<N> on <remote>/main and commit "Release batch-<N>: <summary>" with only Cargo.toml and Cargo.lock. Then refuse unless the new version differs from the last v* tag's and Cargo.lock's pomb entry matches it, and refuse when batch-<N> or v<version> already exists locally or on the remote.
3. Take the release lock .dev/release.lock in the main checkout, which names the batch and the pid and which dev land waits on, and drop it on every exit and once the push is done. Refuse when a live release holds it. Run dev lint --remote, dev test --remote, dev nixcheck --remote and dev build --remote through the release_gate helper at the same time on ci, each in its own gate worktree gate-<N>-<gate> of the release commit, logging to a temp dir, so the lock lasts about the slowest gate and is held until the push. A gate refused because the ci hub is draining is run again alone after POMB_DRAIN_INTERVAL seconds (default 60) until POMB_DRAIN_BOUND seconds (default 3600) have passed. Every gate is waited out first. Any other failing gate aborts before the push, and so does a gate whose ci run is not a new passed run from a clean local source at the gate worktree's HEAD (local@ and its first 12 characters) started by the gate's own agent, pomb/release/<N>/<gate>.
4. Refuse attribution trailers in the new commit and refuse when <remote>/main moved since step 1, then push to main on the remote of record.
5. Tag batch-<N> and v<version> at the pushed commit with the push_tag helper, then compute the nix hash of the commit's tree and print the request block. A failed tag prints the `dev -l .dev/helpers/push_tag.luau` command to rerun for it.
The remote of record is forge.remote, else gerrit when that remote exists, else origin. On gerrit the release commit carries a Change-Id trailer.
The --hash-only <sha> flag fetches main from the remote of record and prints the request block for an already pushed sha: the recovery path when the hash step fails after the push.
The --dry-run flag does everything except the push, the tags and the hash. The release worktree stays behind. dev verify --cleanup removes it once the release is live.
Repeat --env to list pomb.service environment changes in the request.
]],
    run = function(opts, _, ctx)
        -- Read the options
        local found = parse(opts, ctx)
        local repo = main_repo_of(ctx.root)

        -- Reprint the request block for a pushed sha
        if found.hash_only ~= "" then
            if not found.hash_only:find("^[0-9a-f]+$") or #found.hash_only ~= 40 then
                die("The --hash-only value must be a full 40 character sha.")
            end
            local remote, err = forge.remote(repo)
            if not remote then
                dev_command.note(err)
                return 1
            end
            if devos.run(git(repo, "fetch", "-q", remote, "main")) ~= 0 then
                die("The fetch of main failed.")
            end
            local hash = nix_hash(repo, found.hash_only)
            if not hash then
                die(
                    ("The nix hash failed for %s. Rerun: dev release --hash-only %s."):format(
                        found.hash_only,
                        found.hash_only
                    )
                )
            end
            request_block(found, found.hash_only, hash)
            return 0
        end

        -- Fetch the remote of record and find main
        local remote, rerr = forge.remote(repo)
        if not remote then
            dev_command.note(rerr)
            return 1
        end
        local fetched = devos.run(git(repo, "fetch", "-q", remote))
        if fetched ~= 0 then
            return fetched or 1
        end
        local base = read_git(repo, "rev-parse", remote .. "/main")

        -- Refuse when main has no commit since the last tag
        local last_tag, code =
            read_git(repo, "describe", "--tags", "--match", "v*", "--abbrev=0", base)
        if code ~= 0 then
            die(("No v* tag is in the history of %s/main."):format(remote))
        end
        if read_git(repo, "rev-list", "--count", last_tag .. ".." .. base) == "0" then
            die(
                ("The %s/main (%s) has no commit since %s. Nothing to release."):format(
                    remote,
                    base:sub(1, 7),
                    last_tag
                )
            )
        end

        -- Make the release worktree and commit the version bump
        local rel = ".dev/worktrees/release-" .. found.batch
        local wt = repo .. "/" .. rel
        if fs.exists(wt) then
            die(("The path %s already exists. Remove it first."):format(rel))
        end
        if
            devos.run(git(repo, "worktree", "add", "-q", "--detach", rel, base), { cwd = repo })
            ~= 0
        then
            die(("The git worktree add failed for %s."):format(rel))
        end
        local bumped
        bumped, code = devos.capture({ "dev", "-l", helper(ctx, "bump_version"), wt }, { cwd = wt })
        if code ~= 0 then
            die(("The version bump failed. The worktree is left at %s."):format(wt))
        end
        local new_version = trim(bumped)
        local message = ("Release batch-%s: %s"):format(found.batch, found.summary)
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

        -- Check the new version against the last tag
        local tag_toml = assert(fs.mktemp({ prefix = "release-tag" }))
        local tagged
        tagged, code = devos.capture(git(wt, "show", last_tag .. ":Cargo.toml"), QUIET)
        if code ~= 0 then
            fs.remove(tag_toml)
            die(("Cannot read Cargo.toml at %s."):format(last_tag))
        end
        assert(fs.write(tag_toml, tagged))
        local guard = devos.run(
            { "dev", "-l", helper(ctx, "version_guard"), tag_toml, "Cargo.toml", "Cargo.lock" },
            { cwd = wt }
        )
        fs.remove(tag_toml)
        if guard ~= 0 then
            die(
                ("The version %s fails the check against %s. The worktree is left at %s."):format(
                    new_version,
                    last_tag,
                    wt
                )
            )
        end

        -- Refuse a tag that is already taken
        for _, tag in ipairs({ "batch-" .. found.batch, "v" .. new_version }) do
            if tag_taken(repo, remote, tag) then
                die(
                    ("Tag %s already exists locally or on %s. The worktree is left at %s."):format(
                        tag,
                        remote,
                        wt
                    )
                )
            end
        end

        -- Take the release lock and name the gate worktrees
        local logs = assert(fs.mktemp({ prefix = "release-" .. found.batch, dir = true }))
        say(("Logs are in %s.\n"):format(logs))
        local holder = lock.holder(repo)
        if holder and lock.alive(holder) then
            die(
                ("Release batch %s (pid %s) already holds the release lock. The worktree is left at %s."):format(
                    holder.batch,
                    holder.pid,
                    wt
                )
            )
        end
        lock.take(repo, found.batch)
        lock_repo = repo
        local gate_trees = {}
        for _, gate in ipairs(GATES) do
            gate_trees[gate] = ("%s/.dev/worktrees/gate-%s-%s"):format(repo, found.batch, gate)
        end

        -- Remove every gate worktree
        local function drop_gate_trees()
            for _, gate in ipairs(GATES) do
                devos.run(git(repo, "worktree", "remove", "--force", gate_trees[gate]), QUIET)
            end
        end

        -- Make a fresh worktree per gate
        drop_gate_trees()
        for _, gate in ipairs(GATES) do
            if
                devos.run(
                    git(repo, "worktree", "add", "-q", "--detach", gate_trees[gate], sha),
                    { cwd = repo }
                ) ~= 0
            then
                drop_gate_trees()
                die(
                    ("The git worktree add failed for %s. The worktree is left at %s."):format(
                        gate_trees[gate],
                        wt
                    )
                )
            end
        end

        -- Run the gates on ci in parallel
        local jobs = {}
        for _, gate in ipairs(GATES) do
            say(("Running dev %s --remote.\n"):format(gate))
            jobs[#jobs + 1] = ("( cd %s && DEV_AGENT=%s dev -l %s %s %s %s; echo $? >%s ) &"):format(
                devos.quote(gate_trees[gate]),
                devos.quote("pomb/release/" .. found.batch .. "/" .. gate),
                devos.quote(helper(ctx, "release_gate")),
                devos.quote(gate_trees[gate]),
                gate,
                devos.quote(logs),
                devos.quote(logs .. "/" .. gate .. ".code")
            )
        end
        devos.run({ "bash", "-c", table.concat(jobs, "\n") .. "\nwait" }, { cwd = repo })
        drop_gate_trees()

        -- Read one of a gate's result files
        local function gate_file(gate, ext)
            return trim(fs.read(("%s/%s.%s"):format(logs, gate, ext)) or "")
        end

        -- Stop on a failed gate and report the passing ones
        local ran = {}
        for _, gate in ipairs(GATES) do
            local gate_code = gate_file(gate, "code")
            if gate_code == "3" then
                die(
                    ("The ci hub was still draining after %s seconds, so dev %s --remote did not run. See %s/%s.log. The worktree is left at %s."):format(
                        gate_file(gate, "ran"),
                        gate,
                        logs,
                        gate,
                        wt
                    )
                )
            elseif gate_code ~= "0" then
                die(
                    ("The command dev %s --remote did not pass on this worktree: %s. See %s/%s.log. The worktree is left at %s."):format(
                        gate,
                        gate_file(gate, "err"),
                        logs,
                        gate,
                        wt
                    )
                )
            end
            ran[gate] = gate_file(gate, "ran")
        end
        for _, gate in ipairs(GATES) do
            say(("The %s ci run is %s.\n"):format(gate, ran[gate]))
        end
        local summary = ""
        for line in (fs.read(logs .. "/test.log") or ""):gmatch("[^\n]+") do
            summary = line:match("Summary.*") or summary
        end
        local passed = "All gates passed."
        if summary ~= "" then
            passed = ("All gates passed. %s."):format(summary)
        end
        say(passed .. "\n")

        -- Check attribution and that main has not moved, then push
        if
            devos.run({ "dev", "-l", helper(ctx, "push_main"), wt, "--check" }, { cwd = wt }) ~= 0
        then
            die(("The attribution check failed. The worktree is left at %s."):format(wt))
        end
        if devos.run(git(repo, "fetch", "-q", remote)) ~= 0 then
            die(("The fetch of %s failed."):format(remote))
        end
        if read_git(repo, "rev-parse", remote .. "/main") ~= base then
            die(
                ("The %s/main branch moved past %s while the gates ran. Rerun dev release. The worktree is left at %s."):format(
                    remote,
                    base:sub(1, 7),
                    wt
                )
            )
        end
        if found.dry_run then
            lock.drop(repo)
            say(("Dry run, not pushed. The release commit %s is in %s.\n"):format(sha, wt))
            return 0
        end
        if devos.run({ "dev", "-l", helper(ctx, "push_main"), wt }, { cwd = wt }) ~= 0 then
            die(("The push failed. The worktree is left at %s."):format(wt))
        end
        lock.drop(repo)

        -- Push the tags and print the request block
        local reruns = {}
        for _, tag in ipairs({ "batch-" .. found.batch, "v" .. new_version }) do
            if devos.run({ "dev", "-l", helper(ctx, "push_tag"), tag, sha }, { cwd = wt }) ~= 0 then
                reruns[#reruns + 1] = ("dev -l .dev/helpers/push_tag.luau %s %s"):format(tag, sha)
            end
        end
        if #reruns > 0 then
            die(
                ("Pushed %s but tagging failed. Rerun: %s. Then run: dev release --hash-only %s."):format(
                    sha,
                    table.concat(reruns, " && "),
                    sha
                )
            )
        end
        local hash = nix_hash(repo, sha)
        if not hash then
            die(
                ("Pushed %s but the nix hash failed. Run: dev release --hash-only %s."):format(
                    sha,
                    sha
                )
            )
        end
        request_block(found, sha, hash)
        return 0
    end,
})
