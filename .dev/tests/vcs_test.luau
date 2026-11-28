local devos = require("@dev/os")
local fs = require("@dev/fs")
local t = require("@dev/testing")
local testenv = require("@dev/testenv")
local gitenv = require("@lib/pomb/gitenv")
local jjenv = require("@lib/pomb/jjenv")
local vcs = require("@lib/pomb/vcs")

devos.defaults.env = gitenv.env

local git = gitenv.git
local Kind = jjenv.Kind

local function with_path(work, body)
    local env = {}
    for name, value in pairs(gitenv.env) do
        env[name] = value
    end
    env.PATH = work .. "/bin:" .. devos.getenv("PATH")
    local saved = devos.defaults.env
    devos.defaults.env = env
    local ok, err = pcall(body)
    devos.defaults.env = saved
    assert(ok, err)
end

local function fixture(kind)
    local dir = assert(fs.realpath(testenv.tempdir()))
    local work = dir .. "/work"
    git(dir, "init", "-q", "--bare", "remote.git")
    git(dir, "clone", "-q", "remote.git", "work")
    git(work, "remote", "rename", "origin", "gerrit")
    git(work, "checkout", "-q", "-b", "main")
    assert(fs.write(work .. "/tracked", "tracked\n"))
    git(work, "add", "tracked")
    git(work, "commit", "-q", "-m", "base")
    local base = git(work, "rev-parse", "HEAD")
    git(work, "push", "-q", "gerrit", "main")
    git(work, "checkout", "-q", "-b", "card")
    assert(fs.write(work .. "/tracked", "card\n"))
    git(work, "commit", "-q", "-am", "card")
    local head = git(work, "rev-parse", "HEAD")
    git(work, "checkout", "-q", "main")
    assert(fs.write(work .. "/other", "other\n"))
    git(work, "add", "other")
    git(work, "commit", "-q", "-m", "moved")
    local moved = git(work, "rev-parse", "HEAD")
    git(work, "push", "-q", "gerrit", "main")
    git(work, "checkout", "-q", "card")
    assert(fs.write(work .. "/.gitignore", "ignored\n"))
    assert(fs.write(work .. "/ignored", "x\n"))
    assert(fs.write(work .. "/extra", "extra\n"))
    if kind == Kind.JJ then
        jjenv.install(work)
    end
    return { work = work, base = base, head = head, moved = moved }
end

local function case(name, body)
    jjenv.case(name, function(kind)
        local fix = fixture(kind)
        with_path(fix.work, function()
            body(fix, kind)
        end)
    end)
end

local function jj_calls(fix)
    return table.concat(testenv.calls(fix.work, "jj"), "\n")
end

case("stage copies the tracked and new files but not the ignored ones", function(fix, kind)
    local tree = assert(fs.realpath(testenv.tempdir()))
    t.eq(vcs.stage(fix.work, tree), 0)
    t.eq(fs.read(tree .. "/tracked"), "card\n")
    t.eq(fs.read(tree .. "/extra"), "extra\n")
    t.ok(not fs.exists(tree .. "/ignored"))
    t.eq(jj_calls(fix):find("file list", 1, true) ~= nil, kind == Kind.JJ)
end)

case("stage skips a listed file that is gone", function(fix)
    assert(fs.remove(fix.work .. "/extra"))
    git(fix.work, "add", "-N", "tracked")
    assert(fs.remove(fix.work .. "/tracked"))
    local tree = assert(fs.realpath(testenv.tempdir()))
    t.eq(vcs.stage(fix.work, tree), 0)
    t.ok(not fs.exists(tree .. "/tracked"))
end)

case("head names the checked-out commit in full", function(fix, kind)
    t.eq(vcs.head(fix.work), fix.head)
    t.eq(jj_calls(fix):find("-r @-", 1, true) ~= nil, kind == Kind.JJ)
end)

t.test("head is nil outside a repository", function()
    local dir = assert(fs.realpath(testenv.tempdir()))
    t.eq(vcs.head(dir), nil)
end)

case("merge_base finds the common ancestor with the remote main", function(fix, kind)
    t.eq(vcs.merge_base(fix.work, "gerrit/main"), fix.base)
    t.eq(jj_calls(fix):find("main@gerrit", 1, true) ~= nil, kind == Kind.JJ)
end)

case("merge_base is nil for an unknown remote branch", function(fix)
    t.eq(vcs.merge_base(fix.work, "gerrit/missing"), nil)
end)

case("is_ancestor holds for a commit on the remote main", function(fix, kind)
    t.eq(vcs.is_ancestor(fix.work, fix.base, "gerrit/main"), true)
    t.eq(vcs.is_ancestor(fix.work, fix.moved, "gerrit/main"), true)
    t.eq(jj_calls(fix):find("::main@gerrit", 1, true) ~= nil, kind == Kind.JJ)
end)

case("is_ancestor fails for a commit the remote main lacks", function(fix)
    t.eq(vcs.is_ancestor(fix.work, fix.head, "gerrit/main"), false)
end)

case("is_ancestor fails for an unknown remote branch", function(fix)
    t.eq(vcs.is_ancestor(fix.work, fix.base, "gerrit/missing"), false)
end)

case("fetch updates the remote branch through its checkout kind", function(fix, kind)
    local dir = fix.work:match("^(.*)/work$")
    git(dir, "clone", "-q", "-b", "main", "remote.git", "other")
    local other = dir .. "/other"
    assert(fs.write(other .. "/newer", "newer\n"))
    git(other, "add", "newer")
    git(other, "commit", "-q", "-m", "newer")
    local newer = git(other, "rev-parse", "HEAD")
    git(other, "push", "-q", "origin", "HEAD:main")
    t.eq(vcs.fetch(fix.work, "gerrit"), 0)
    t.eq(git(fix.work, "rev-parse", "gerrit/main") == newer, kind == Kind.GIT)
    t.eq(jj_calls(fix):find("git fetch --remote gerrit", 1, true) ~= nil, kind == Kind.JJ)
end)

case("resolve names a revision as a full commit id", function(fix, kind)
    t.eq(vcs.resolve(fix.work, "gerrit/main"), fix.moved)
    t.eq(vcs.resolve(fix.work, fix.base), fix.base)
    t.eq(jj_calls(fix):find("-r main@gerrit", 1, true) ~= nil, kind == Kind.JJ)
end)

case("resolve is nil for a revision that names no commit", function(fix)
    t.eq(vcs.resolve(fix.work, "nowhere"), nil)
end)

case("export writes the files of REV under the prefix", function(fix, kind)
    local tree = assert(fs.realpath(testenv.tempdir()))
    t.eq(vcs.export(fix.work, fix.moved, tree, "base/"), 0)
    t.eq(fs.read(tree .. "/base/tracked"), "tracked\n")
    t.eq(fs.read(tree .. "/base/other"), "other\n")
    t.ok(not fs.exists(tree .. "/base/extra"))
    t.ok(not fs.exists(tree .. "/base/ignored"))
    t.eq(jj_calls(fix):find("file show", 1, true) ~= nil, kind == Kind.JJ)
end)

case("export fails for a revision that does not exist", function(fix)
    local tree = assert(fs.realpath(testenv.tempdir()))
    t.ok(vcs.export(fix.work, "nowhere", tree, "base/") ~= 0)
end)
