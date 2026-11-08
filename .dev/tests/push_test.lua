local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")

local ROOT = fs.dirname(fs.dirname(fs.dirname(debug.getinfo(1, "S").source:sub(2))))

devos.defaults.env = {
    GIT_CONFIG_GLOBAL = "/dev/null",
    GIT_CONFIG_NOSYSTEM = "1",
    GIT_AUTHOR_NAME = "t",
    GIT_AUTHOR_EMAIL = "t@t",
    GIT_COMMITTER_NAME = "t",
    GIT_COMMITTER_EMAIL = "t@t",
}

local CHANGE_ID = "Change-Id: I0123456789abcdef0123456789abcdef01234567"

local function git(dir, ...)
    local out, code = devos.capture({ "git", "-C", dir, ... })
    assert(code == 0, table.concat({ ... }, " "))
    return (out:gsub("%s+$", ""))
end

local function commit(dir, message)
    git(dir, "commit", "-q", "--allow-empty", "-m", message)
end

local function fixture()
    local dir = assert(fs.realpath(testenv.tempdir()))
    local remote = dir .. "/remote.git"
    local work = dir .. "/work"
    git(dir, "init", "-q", "--bare", "remote.git")
    git(dir, "clone", "-q", "remote.git", "work")
    git(work, "checkout", "-q", "-b", "main")
    commit(work, "base")
    git(work, "push", "-q", "origin", "main")
    assert(fs.mkdir(work .. "/bin"))
    local guard = work .. "/bin/jj-git-guard"
    assert(fs.write(guard, '#!/bin/sh\nshift 2\nexec git push "$@"\n'))
    assert(devos.run({ "chmod", "755", guard }) == 0)
    assert(fs.symlink(ROOT .. "/.dev", work .. "/.dev"))
    return { dir = dir, work = work, remote = remote }
end

local function helper(fix, ...)
    return testenv.verb(fix.work, { ... })
end

local function remote_main(fix)
    return git(fix.remote, "rev-parse", "refs/heads/main")
end

t.test("push_main pushes HEAD to main when clean", function()
    local fix = fixture()
    commit(fix.work, "add a change")
    local _, code, err = helper(fix, "__push_main", fix.work)
    t.ok(code == 0, err)
    t.eq(remote_main(fix), git(fix.work, "rev-parse", "HEAD"))
    fs.remove(fix.dir, { recursive = true })
end)

t.test("push_main exits three when the remote refuses non-fast-forward", function()
    local fix = fixture()
    local other = fix.dir .. "/other"
    git(fix.dir, "clone", "-q", "-b", "main", "remote.git", "other")
    commit(other, "main moved on")
    git(other, "push", "-q", "origin", "HEAD:refs/heads/main")
    commit(fix.work, "add a change")
    local _, code, err = helper(fix, "__push_main", fix.work)
    t.eq(code, 3, err)
    t.eq(remote_main(fix), git(other, "rev-parse", "HEAD"))
    fs.remove(fix.dir, { recursive = true })
end)

t.test("push_main exits one for a Claude-Session trailer", function()
    local fix = fixture()
    commit(fix.work, "add a change\n\nClaude-Session: abc123")
    local _, code, err = helper(fix, "__push_main", fix.work)
    t.eq(code, 1, err)
    t.ok(err:find("Claude-Session", 1, true), err)
    fs.remove(fix.dir, { recursive = true })
end)

t.test("push_main exits one for a trailer other than Change-Id", function()
    local fix = fixture()
    commit(fix.work, "add a change\n\nReviewed-by: Someone <someone@example.com>")
    local _, code, err = helper(fix, "__push_main", fix.work)
    t.eq(code, 1, err)
    t.ok(err:find("Reviewed-by", 1, true), err)
    fs.remove(fix.dir, { recursive = true })
end)

t.test("push_main accepts a Change-Id trailer", function()
    local fix = fixture()
    commit(fix.work, "add a change\n\n" .. CHANGE_ID)
    local _, code, err = helper(fix, "__push_main", fix.work, "--check")
    t.eq(code, 0, err)
    t.eq(remote_main(fix), git(fix.work, "rev-parse", "HEAD~1"))
    fs.remove(fix.dir, { recursive = true })
end)

t.test("push_tag tags a commit on main and pushes the tag", function()
    local fix = fixture()
    local sha = git(fix.work, "rev-parse", "HEAD")
    local out, code, err = helper(fix, "__push_tag", "v1.2.3", sha)
    t.eq(code, 0, err)
    t.eq(out, "Tagged v1.2.3 at " .. sha .. ".\n")
    t.eq(git(fix.remote, "rev-parse", "v1.2.3^{commit}"), sha)
    fs.remove(fix.dir, { recursive = true })
end)

t.test("push_tag refuses a tag that exists on the remote", function()
    local fix = fixture()
    local sha = git(fix.work, "rev-parse", "HEAD")
    t.eq(select(2, helper(fix, "__push_tag", "v1.2.3", sha)), 0)
    git(fix.work, "tag", "-d", "v1.2.3")
    local _, code, err = helper(fix, "__push_tag", "v1.2.3", sha)
    t.eq(code, 1)
    t.ok(err:find("Tag v1.2.3 already exists.", 1, true), err)
    fs.remove(fix.dir, { recursive = true })
end)

t.test("push_tag refuses a commit that is not on main", function()
    local fix = fixture()
    commit(fix.work, "unpushed")
    local sha = git(fix.work, "rev-parse", "HEAD")
    local _, code, err = helper(fix, "__push_tag", "v1.2.3", sha)
    t.eq(code, 1)
    t.ok(err:find("is not on origin/main.", 1, true), err)
    fs.remove(fix.dir, { recursive = true })
end)

t.test("delete_remote_branch prints the tip and deletes the branch", function()
    local fix = fixture()
    git(fix.work, "push", "-q", "origin", "HEAD:refs/heads/topic")
    local tip = git(fix.remote, "rev-parse", "refs/heads/topic")
    local out, code, err = helper(fix, "__delete_remote_branch", "topic")
    t.eq(code, 0, err)
    t.eq(out, "Deleting origin/topic at " .. tip .. ".\n")
    local _, shown = devos.capture({
        "git",
        "-C",
        fix.remote,
        "rev-parse",
        "-q",
        "--verify",
        "refs/heads/topic",
    })
    t.eq(shown, 1)
    fs.remove(fix.dir, { recursive = true })
end)

t.test("delete_remote_branch refuses main and a missing branch", function()
    local fix = fixture()
    local _, code, err = helper(fix, "__delete_remote_branch", "main")
    t.eq(code, 1)
    t.ok(err:find("Refused to delete main.", 1, true), err)
    _, code, err = helper(fix, "__delete_remote_branch", "nope")
    t.eq(code, 1)
    t.ok(err:find("No branch nope exists on origin.", 1, true), err)
    t.eq(remote_main(fix), git(fix.work, "rev-parse", "HEAD"))
    fs.remove(fix.dir, { recursive = true })
end)
