local backup = require("pomb.backup")
local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")
local gitenv = require("pomb.gitenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

devos.defaults.env = gitenv.env

local git = gitenv.git

local STUB_HASH = "0123456789abcdef"

local function release_fixture()
    local dir = assert(fs.realpath(testenv.tempdir()))
    local work = dir .. "/work"
    git(dir, "init", "-q", "--bare", "remote.git")
    git(dir, "clone", "-q", "remote.git", "work")
    git(work, "remote", "rename", "origin", "gerrit")
    git(work, "checkout", "-q", "-b", "main")
    assert(fs.mkdir(work .. "/.dev"))
    for _, name in ipairs(assert(fs.list(ROOT .. "/.dev"))) do
        if name ~= "worktrees" then
            assert(fs.symlink(ROOT .. "/.dev/" .. name, work .. "/.dev/" .. name))
        end
    end
    assert(fs.mkdir(work .. "/static"))
    assert(fs.write(work .. "/static/app.js", "app\n"))
    git(work, "add", "static/app.js")
    git(work, "commit", "-q", "-m", "release")
    git(work, "push", "-q", "gerrit", "main")
    return dir, work
end

local login_reply = "HTTP/2 200\\r\\n\\r\\n"

local function write_stubs(dir, ssh_body)
    local bin = dir .. "/bin"
    assert(fs.mkdir(bin, { parents = true }))
    local ssh = ssh_body or ("#!/bin/sh\necho ran >>'" .. dir .. "/ssh.log'\n")
    assert(fs.write(bin .. "/ssh", ssh))
    assert(fs.write(
        bin .. "/curl",
        "#!/bin/sh\necho \"$@\" >>'" .. dir .. "/curl.log'\nprintf '" .. login_reply .. "'\n"
    ))
    assert(fs.write(bin .. "/shasum", "#!/bin/sh\ncat >/dev/null\necho " .. STUB_HASH .. "  -\n"))
    assert(devos.run({ "chmod", "755", bin .. "/ssh", bin .. "/curl", bin .. "/shasum" }) == 0)
    return bin
end

local function verify_with_fake_ssh(args)
    local dir = assert(fs.realpath(testenv.tempdir()))
    local bin = write_stubs(dir)
    local _, code, err = testenv.verb(ROOT, args, {
        env = { PATH = bin .. ":" .. os.getenv("PATH") },
    })
    local ssh = fs.read(dir .. "/ssh.log")
    fs.remove(dir, { recursive = true })
    return code, err, ssh
end

local function passing_ssh_body(sha)
    local hash = STUB_HASH
    return "#!/bin/sh\n"
        .. "case \"$*\" in\n"
        .. "*version*) echo '{\"build_sha\":\"" .. sha .. "\"}' ;;\n"
        .. "*app.js*) echo " .. hash .. " ;;\n"
        .. "*http_code*) printf 403 ;;\n"
        .. "*pomb_nope*) echo '{\"code\":\"token_invalid\"}' ;;\n"
        .. "*openapi*) echo '{\"servers\":[{\"url\":\"https://pomb.chip.network\"}]}' ;;\n"
        .. "*journalctl*) echo 'pomb: schema v1'; echo 'pomb: listening on 3000' ;;\n"
        .. "*sudo\\ test*) exit 0 ;;\n"
        .. "esac\n"
end

local function verify_on_release_fixture(args, ssh_for, setup)
    local dir, work = release_fixture()
    if setup then
        setup(work)
    end
    local head = devos.capture({ "git", "-C", work, "rev-parse", "HEAD" })
    local bin = write_stubs(dir, ssh_for and ssh_for(head:match("%x+")))
    local argv = { "dev", "verify" }
    for _, word in ipairs(args or { "HEAD" }) do
        argv[#argv + 1] = word
    end
    local both = dir .. "/both.txt"
    local code = devos.run(argv, {
        cwd = work,
        env = { PATH = bin .. ":" .. os.getenv("PATH") },
        stdout = both,
        stderr = "stdout",
    })
    local text = assert(fs.read(both))
    local curl = fs.read(dir .. "/curl.log")
    fs.remove(dir, { recursive = true })
    return code, text, curl
end

t.test("backup resolves a bare file name under the backup directory", function()
    local path, why = backup.resolve("pomb-1.redb")
    t.eq(path, "/var/lib/pomb-backups/pomb-1.redb")
    t.eq(why, nil)
end)

t.test("backup resolves a full path under the backup directory", function()
    local path, why = backup.resolve("/var/lib/pomb-backups/pomb-1.redb")
    t.eq(path, "/var/lib/pomb-backups/pomb-1.redb")
    t.eq(why, nil)
end)

t.test("backup refuses an absolute path outside the backup directory", function()
    local path, why = backup.resolve("/etc/passwd")
    t.eq(path, nil)
    local expected = "The --backup argument must be a file name or a path under "
        .. "/var/lib/pomb-backups, not /etc/passwd."
    t.eq(why, expected)
end)

t.test("backup refuses a path that climbs out of the backup directory", function()
    local path = backup.resolve("/var/lib/pomb-backups/../secrets")
    t.eq(path, nil)
    t.eq(backup.resolve("../secrets"), nil)
end)

t.test("backup refuses an empty argument and the bare directory", function()
    t.eq(backup.resolve(""), nil)
    t.eq(backup.resolve("/var/lib/pomb-backups/"), nil)
end)

t.test("bare_backup_usage refuses a --backup with no value before any ssh check", function()
    local cases = {
        { "verify", "HEAD", "--backup" },
        { "verify", "HEAD", "--backup", "" },
        { "verify", "HEAD", "--backup", "--cleanup", ".dev/worktrees/x" },
    }
    for _, args in ipairs(cases) do
        local code, err, ssh = verify_with_fake_ssh(args)
        t.eq(code, 2)
        t.ok(err:find("Usage: dev verify takes a release sha", 1, true), err)
        t.eq(ssh, nil)
    end
end)

t.test("verify_login_probe_should_use_the_curl_on_path", function()
    local code, text, curl = verify_on_release_fixture()
    t.eq(code, 1)
    t.ok(curl ~= nil and curl:find("https://pomb.chip.network/login", 1, true) ~= nil, text)
end)

t.test("verify_summary_should_follow_every_check_line_in_one_file", function()
    local code, text = verify_on_release_fixture()
    local lines = {}
    for line in text:gmatch("[^\n]+") do
        lines[#lines + 1] = line
    end
    t.eq(code, 1)
    local expected = "check(s) failed, so the worktrees are untouched."
    t.ok(lines[#lines]:find(expected, 1, true) ~= nil, text)
    local checks = 0
    for i, line in ipairs(lines) do
        if line:match("^PASS ") or line:match("^FAIL ") then
            checks = checks + 1
            t.ok(i < #lines, text)
        end
    end
    t.ok(checks > 0, text)
end)

local function is_sentence(line)
    return line:match("^[A-Z%d].*%.$") ~= nil and not line:find(";", 1, true)
end

local function assert_every_line_is_a_sentence(text)
    for line in text:gmatch("[^\n]+") do
        if line:sub(1, 1) ~= " " then
            local body = line:match("^PASS (.+)$") or line:match("^FAIL (.+)$") or line
            t.ok(is_sentence(body), line)
        end
    end
end

t.test("verify_messages_should_be_full_sentences_on_every_printed_line", function()
    local _, text = verify_on_release_fixture()
    local _, usage = verify_with_fake_ssh({ "verify", "HEAD", "--backup" })
    assert_every_line_is_a_sentence(text .. "\n" .. usage)
end)

t.test("verify_messages_should_be_full_sentences_when_every_check_passes", function()
    local code, text = verify_on_release_fixture({
        "HEAD",
        "--backup",
        "pomb-1.redb",
        "--cleanup",
        ".dev/worktrees/absent",
        "/elsewhere/x",
    }, passing_ssh_body)
    t.eq(code, 0)
    t.ok(text:find("All checks passed.", 1, true) ~= nil, text)
    t.ok(text:find("Skipped .dev/worktrees/absent", 1, true) ~= nil, text)
    t.ok(text:find("Skipped /elsewhere/x", 1, true) ~= nil, text)
    assert_every_line_is_a_sentence(text)
end)

t.test("verify_messages_should_be_full_sentences_when_the_commit_is_missing", function()
    local code, text = verify_on_release_fixture({ "nope" })
    t.eq(code, 1)
    t.ok(text:find("No commit nope exists.", 1, true) ~= nil, text)
    assert_every_line_is_a_sentence(text)
end)

local function add_worktrees(work)
    git(work, "worktree", "add", "-q", "-b", "merged-branch", ".dev/worktrees/merged", "HEAD")
    git(work, "worktree", "add", "-q", "-b", "kept-branch", ".dev/worktrees/kept", "HEAD")
    git(work, "worktree", "add", "-q", "-b", "forced-branch", ".dev/worktrees/forced", "HEAD")
    git(work .. "/.dev/worktrees/kept", "commit", "-q", "--allow-empty", "-m", "kept")
    git(work .. "/.dev/worktrees/forced", "commit", "-q", "--allow-empty", "-m", "forced")
end

t.test("verify_messages_should_be_full_sentences_when_a_real_worktree_is_cleaned_up", function()
    local code, text = verify_on_release_fixture({
        "HEAD",
        "--cleanup",
        ".dev/worktrees/merged",
        ".dev/worktrees/kept",
        ".dev/worktrees/forced",
        "--delete-branch",
        "forced-branch",
    }, passing_ssh_body, add_worktrees)
    t.eq(code, 0)
    local expected = {
        "Removed .dev/worktrees/merged.",
        "Deleted branch merged-branch, which was merged.",
        "Removed .dev/worktrees/kept.",
        "Kept branch kept-branch, which is not merged, so pass --delete-branch kept-branch to delete it.",
        "Removed .dev/worktrees/forced.",
        "Deleted branch forced-branch by force.",
    }
    for _, line in ipairs(expected) do
        t.ok(text:find(line, 1, true) ~= nil, text)
    end
    assert_every_line_is_a_sentence(text)
end)

t.test("verify_messages_should_be_full_sentences_when_the_backup_is_refused", function()
    local code, text = verify_on_release_fixture({ "HEAD", "--backup", "/etc/passwd" })
    t.eq(code, 1)
    t.ok(text:find("The --backup argument must be a file name", 1, true) ~= nil, text)
    assert_every_line_is_a_sentence(text)
end)

t.test("verify_messages_should_be_full_sentences_when_it_cannot_run", function()
    local code, text = verify_on_release_fixture({ "HEAD" }, nil, function(work)
        git(work, "config", "forge.remote", "nosuch")
    end)
    t.eq(code, 1)
    t.ok(text:find("Dev verify cannot run, because", 1, true) ~= nil, text)
    assert_every_line_is_a_sentence(text)
end)

t.test("verify_messages_should_be_full_sentences_for_every_login_answer", function()
    local replies = {
        { "HTTP/2 303\\r\\nlocation: /\\r\\n\\r\\n", 0, "redirected to /, with no sign-in" },
        { "HTTP/2 303\\r\\nlocation: /setup\\r\\n\\r\\n", 0, "redirected to /setup" },
        { "HTTP/2 500\\r\\n\\r\\n", 1, "answered with status 500 and location none" },
    }
    for _, reply in ipairs(replies) do
        login_reply = reply[1]
        local code, text = verify_on_release_fixture({ "HEAD" }, passing_ssh_body)
        t.eq(code, reply[2])
        t.ok(text:find(reply[3], 1, true) ~= nil, text)
        assert_every_line_is_a_sentence(text)
    end
    login_reply = "HTTP/2 200\\r\\n\\r\\n"
end)

t.test("verify_messages_should_be_full_sentences_when_the_backup_is_missing", function()
    local missing = function(sha)
        return (passing_ssh_body(sha):gsub("exit 0", "exit 1"))
    end
    local code, text = verify_on_release_fixture({ "HEAD", "--backup", "pomb-1.redb" }, missing)
    t.eq(code, 1)
    t.ok(text:find("is missing from the host.", 1, true) ~= nil, text)
    assert_every_line_is_a_sentence(text)
end)
