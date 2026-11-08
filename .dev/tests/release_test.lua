local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

devos.defaults.env = {
    GIT_CONFIG_GLOBAL = "/dev/null",
    GIT_CONFIG_NOSYSTEM = "1",
    GIT_AUTHOR_NAME = "t",
    GIT_AUTHOR_EMAIL = "t@example.com",
    GIT_COMMITTER_NAME = "t",
    GIT_COMMITTER_EMAIL = "t@example.com",
}

local RELEASE = { "release", "--batch", "7", "--summary", "cut it" }
local LOCAL, REMOTE = "local", "remote"
local FILES = {
    "release.lua",
    "__gate.lua",
    "__push_main.lua",
    "__push_tag.lua",
    "lua/pomb/forge.lua",
    "__bump_version.lua",
    "__version_guard.lua",
}

local STUB_DEV = [[
case "$1" in __*) exec luajit '@main@' "$@";; esac
echo "$1" >>'@dir@/gates.log'
[ -z "$MOVE_MAIN" ] || git -C "$MOVE_MAIN" push -q origin HEAD:main
[ "$1" != "$FAIL_GATE" ] || exit 1
touch '@dir@/runs.jsonl'
printf '{"run":"run%s","repo":"%s-0123","source":"local","kind":"run","status":"passed"}\n' "$(wc -l <'@dir@/runs.jsonl' | tr -d ' ')" "$(basename "$PWD")" >>'@dir@/runs.jsonl'
echo 'Summary [1s] 1 tests run: 1 passed'
]]

local STUB_CI = [[
touch '@dir@/runs.jsonl'
jq -s '{protocol: 48, runs: .}' '@dir@/runs.jsonl'
]]

local STUB_GUARD = [[
[ "$1" != push ] || shift
[ "$1" != -- ] || shift
exec git push "$@"
]]

local function manifest(version)
    return ('[package]\nname = "pomb"\nversion = "%s"\nedition = "2024"\n'):format(version)
end

local function lock(version)
    return ('[[package]]\nname = "other"\nversion = "9.9.9"\n\n[[package]]\nname = "pomb"\nversion = "%s"\n'):format(
        version
    )
end

local function git(dir, ...)
    local out, code = devos.capture({ "git", "-C", dir, ... })
    assert(code == 0, table.concat({ ... }, " "))
    return (out:gsub("%s+$", ""))
end

local function launcher()
    local found = assert(package.searchpath("dev.cli", package.path))
    return (assert(fs.realpath(found)):gsub("/lua/dev/cli%.lua$", "/main.lua"))
end

local function stub(dir, name, body)
    local path = dir .. "/bin/" .. name
    local text = body:gsub("@(%w+)@", { dir = dir, main = launcher() })
    assert(fs.write(path, "#!/bin/sh\n" .. text))
    assert(devos.run({ "chmod", "755", path }) == 0)
end

local function commit_version(repo, version, message)
    assert(fs.write(repo .. "/Cargo.toml", manifest(version)))
    assert(fs.write(repo .. "/Cargo.lock", lock(version)))
    git(repo, "add", "Cargo.toml", "Cargo.lock")
    git(repo, "commit", "-q", "-m", message)
end

local function commit_card(repo, name)
    assert(fs.write(repo .. "/" .. name, "card\n"))
    git(repo, "add", name)
    git(repo, "commit", "-q", "-m", "Add " .. name)
end

local function fixture(tagged, current, cards)
    local dir = assert(fs.realpath(testenv.tempdir()))
    local repo = dir .. "/repo"
    git(dir, "init", "-q", "--bare", "-b", "main", "remote.git")
    git(dir, "init", "-q", "-b", "main", "repo")
    git(repo, "remote", "add", "origin", dir .. "/remote.git")
    for _, file in ipairs(FILES) do
        local to = repo .. "/.dev/" .. file
        assert(fs.mkdir(fs.dirname(to), { parents = true }))
        assert(fs.write(to, assert(fs.read(ROOT .. "/.dev/" .. file))))
    end
    git(repo, "add", "-f", ".dev")
    git(repo, "commit", "-q", "-m", "Add dev verbs")
    commit_version(repo, tagged, "Release batch-1: first")
    git(repo, "tag", "-a", "v" .. tagged, "-m", "release")
    for n = 0, cards - 1 do
        commit_card(repo, "card" .. n)
    end
    if current ~= tagged then
        commit_version(repo, current, "Change the version by hand")
    end
    git(repo, "push", "-q", "origin", "main", "--tags")
    assert(fs.write(repo .. "/.git/info/exclude", ".dev/\n"))
    assert(fs.mkdir(dir .. "/bin"))
    stub(dir, "dev", STUB_DEV)
    stub(dir, "ci", STUB_CI)
    stub(dir, "jj-git-guard", STUB_GUARD)
    stub(dir, "nix", "echo sha256-stubhash\n")
    return { dir = dir, repo = repo, remote = dir .. "/remote.git" }
end

local function release(fix, args, env)
    local vars = { TMPDIR = fix.dir, PATH = fix.dir .. "/bin:" .. os.getenv("PATH") }
    for name, value in pairs(env or {}) do
        vars[name] = value
    end
    return testenv.verb(fix.repo, args, { env = vars })
end

local function remote_ref(fix, spec)
    return git(fix.remote, "rev-parse", spec)
end

local function gates(fix)
    local found = {}
    for line in (fs.read(fix.dir .. "/gates.log") or ""):gmatch("[^\n]+") do
        found[#found + 1] = line
    end
    return table.concat(found, ",")
end

local function with(list, ...)
    local joined = {}
    for i, item in ipairs(list) do
        joined[i] = item
    end
    for _, word in ipairs({ ... }) do
        joined[#joined + 1] = word
    end
    return joined
end

local function tags(repo)
    return table.concat(
        (function()
            local found = {}
            for line in git(repo, "tag", "-l"):gmatch("[^\n]+") do
                found[#found + 1] = line
            end
            return found
        end)(),
        ","
    )
end

local function done(fix)
    fs.remove(fix.dir, { recursive = true })
end

t.test("release should commit only the version bump on the remote main", function()
    local fix = fixture("0.5.85", "0.5.85", 2)
    local card_head = remote_ref(fix, "main")
    local out, code, err = release(fix, RELEASE)
    t.eq(code, 0, err)
    local head = remote_ref(fix, "main")
    t.eq(remote_ref(fix, "main~1"), card_head)
    t.eq(git(fix.remote, "log", "-1", "--format=%s", "main"), "Release batch-7: cut it")
    t.eq(git(fix.remote, "diff", "--name-only", "main~1", "main"), "Cargo.lock\nCargo.toml")
    t.ok(git(fix.remote, "show", "main:Cargo.toml"):find('version = "0.5.86"', 1, true))
    t.eq(gates(fix), "lint,test,nixcheck,build")
    t.ok(out:find("Pomb sha: " .. head, 1, true), out)
    t.ok(out:find("sha256-stubhash", 1, true), out)
    done(fix)
end)

t.test("release should refuse when main has no commit since the last tag", function()
    local fix = fixture("0.5.85", "0.5.85", 0)
    local before = remote_ref(fix, "main")
    local _, code, err = release(fix, RELEASE)
    t.ok(code ~= 0)
    t.ok(err:find("no commit since v0.5.85", 1, true), err)
    t.eq(gates(fix), "")
    t.eq(remote_ref(fix, "main"), before)
    done(fix)
end)

t.test("release should refuse a version equal to the last tag", function()
    local fix = fixture("0.5.85", "0.5.84", 1)
    local before = remote_ref(fix, "main")
    local _, code, err = release(fix, RELEASE)
    t.ok(code ~= 0)
    t.ok(err:find("equals the base version 0.5.85", 1, true), err)
    t.eq(gates(fix), "")
    t.eq(remote_ref(fix, "main"), before)
    done(fix)
end)

t.test("release should stop before the push when a gate fails", function()
    local fix = fixture("0.5.85", "0.5.85", 1)
    local before = remote_ref(fix, "main")
    local _, code, err = release(fix, RELEASE, { FAIL_GATE = "test" })
    t.ok(code ~= 0)
    t.ok(err:find("dev test --remote", 1, true), err)
    t.eq(gates(fix), "lint,test")
    t.eq(remote_ref(fix, "main"), before)
    done(fix)
end)

t.test("release dry run should skip the push and hash only should print the block", function()
    local fix = fixture("0.5.85", "0.5.85", 1)
    local before = remote_ref(fix, "main")
    local dry, code, err = release(fix, with(RELEASE, "--dry-run"))
    t.eq(code, 0, err)
    t.ok(dry:find("dry run, not pushed", 1, true), dry)
    t.ok(not dry:find("homelab request", 1, true))
    t.eq(gates(fix), "lint,test,nixcheck,build")
    t.eq(remote_ref(fix, "main"), before)
    local out
    out, code, err = release(fix, { "release", "--hash-only", before, "--env", "KEY=VALUE" })
    t.eq(code, 0, err)
    t.ok(out:find("Pomb sha: " .. before, 1, true), out)
    t.ok(out:find("nix hash:     sha256-stubhash", 1, true), out)
    t.ok(out:find("  KEY=VALUE", 1, true), out)
    done(fix)
end)

t.test("release should exit 2 with the usage for a commit argument", function()
    local fix = fixture("0.5.85", "0.5.85", 1)
    local _, code, err = release(fix, { "release", "HEAD", "--batch", "7", "--summary", "cut it" })
    t.eq(code, 2)
    t.ok(err:find("usage: dev release --batch N", 1, true), err)
    t.eq(gates(fix), "")
    done(fix)
end)

t.test("release should refuse to push when main moved during the gates", function()
    local fix = fixture("0.5.85", "0.5.85", 1)
    git(fix.dir, "clone", "-q", "-b", "main", fix.remote, "rival")
    local rival = fix.dir .. "/rival"
    commit_card(rival, "late")
    local _, code, err = release(fix, RELEASE, { MOVE_MAIN = rival })
    t.ok(code ~= 0)
    t.ok(err:find("moved past", 1, true), err)
    t.eq(remote_ref(fix, "main"), git(rival, "rev-parse", "HEAD"))
    done(fix)
end)

t.test("release should tag batch and version at the pushed sha", function()
    local fix = fixture("0.5.85", "0.5.85", 2)
    local _, code, err = release(fix, RELEASE)
    t.eq(code, 0, err)
    local head = remote_ref(fix, "main")
    t.eq(remote_ref(fix, "refs/tags/batch-7^{commit}"), head)
    t.eq(remote_ref(fix, "refs/tags/v0.5.86^{commit}"), head)
    done(fix)
end)

t.test("release should not tag on a dry run", function()
    local fix = fixture("0.5.85", "0.5.85", 1)
    local _, code, err = release(fix, with(RELEASE, "--dry-run"))
    t.eq(code, 0, err)
    t.eq(tags(fix.remote), "v0.5.85")
    t.eq(tags(fix.repo), "v0.5.85")
    done(fix)
end)

t.test("release should print the tag rerun command when the tag push fails", function()
    local fix = fixture("0.5.85", "0.5.85", 1)
    local hook = fix.remote .. "/hooks/pre-receive"
    assert(fs.write(hook, '#!/bin/sh\nwhile read o n r; do [ "$r" != refs/tags/batch-7 ] || exit 1; done\n'))
    assert(devos.run({ "chmod", "755", hook }) == 0)
    local _, code, err = release(fix, RELEASE)
    t.ok(code ~= 0)
    local head = remote_ref(fix, "main")
    t.ok(err:find("dev __push_tag batch-7 " .. head, 1, true), err)
    t.eq(git(fix.repo, "tag", "-l", "batch-7"), "")
    t.eq(git(fix.remote, "tag", "-l", "batch-7"), "")
    t.eq(remote_ref(fix, "refs/tags/v0.5.86^{commit}"), head)
    done(fix)
end)

t.test("release should print the version tag rerun command when only that tag push fails", function()
    local fix = fixture("0.5.85", "0.5.85", 1)
    local hook = fix.remote .. "/hooks/pre-receive"
    assert(fs.write(hook, '#!/bin/sh\nwhile read o n r; do [ "$r" != refs/tags/v0.5.86 ] || exit 1; done\n'))
    assert(devos.run({ "chmod", "755", hook }) == 0)
    local out, code, err = release(fix, RELEASE)
    t.ok(code ~= 0)
    local head = remote_ref(fix, "main")
    t.ok(err:find("rerun: dev __push_tag v0.5.86 " .. head, 1, true), err)
    t.ok(not err:find("dev __push_tag batch-7", 1, true), err)
    t.eq(remote_ref(fix, "refs/tags/batch-7^{commit}"), head)
    t.ok(not out:find("homelab request", 1, true), out)
    done(fix)
end)

local function refuse_existing_tag(opts)
    local fix = fixture("0.5.85", "0.5.85", 1)
    local before = remote_ref(fix, "main")
    local dir = opts.on == LOCAL and fix.repo or fix.remote
    local target = git(
        dir,
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit-tree",
        "main^{tree}",
        "-p",
        "main",
        "-m",
        "side"
    )
    git(dir, "tag", opts.tag, target)
    local _, code, err = release(fix, RELEASE)
    t.ok(code ~= 0)
    t.ok(err:find("Tag " .. opts.tag .. " already exists", 1, true), err)
    t.eq(gates(fix), "")
    t.eq(remote_ref(fix, "main"), before)
    done(fix)
end

t.test("release should refuse a batch tag that exists locally before the push", function()
    refuse_existing_tag({ tag = "batch-7", on = LOCAL })
end)

t.test("release should refuse a batch tag that exists on the remote before the push", function()
    refuse_existing_tag({ tag = "batch-7", on = REMOTE })
end)

t.test("release should refuse a version tag that exists locally before the push", function()
    refuse_existing_tag({ tag = "v0.5.86", on = LOCAL })
end)

t.test("release should refuse a version tag that exists on the remote before the push", function()
    refuse_existing_tag({ tag = "v0.5.86", on = REMOTE })
end)

t.test("release should print the block after both tags are pushed", function()
    local fix = fixture("0.5.85", "0.5.85", 2)
    local out, code, err = release(fix, RELEASE)
    t.eq(code, 0, err)
    local head = remote_ref(fix, "main")
    t.eq(remote_ref(fix, "refs/tags/batch-7^{commit}"), head)
    t.eq(remote_ref(fix, "refs/tags/v0.5.86^{commit}"), head)
    t.ok(out:find("homelab request", 1, true), out)
    t.ok(out:find("Pomb sha: " .. head, 1, true), out)
    t.ok(out:find("batch:        7", 1, true), out)
    done(fix)
end)
