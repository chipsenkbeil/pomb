local devos = require("dev.os")
local forge = require("pomb.forge")
local fs = require("dev.fs")
local t = require("dev.testing")
local testenv = require("dev.testenv")
local gitenv = require("pomb.gitenv")

devos.defaults.env = gitenv.env

local git = gitenv.git

local function raw_commit(root)
    local out, code = devos.capture({ "git", "-C", root, "cat-file", "commit", "HEAD" })
    assert(code == 0, "cat-file commit HEAD")
    return out
end

local function repo(remotes, config)
    local root = assert(fs.realpath(testenv.tempdir()))
    git(root, "init", "-q")
    for _, name in ipairs(remotes) do
        git(root, "remote", "add", name, "/srv/git/r.git")
    end
    for _, pair in ipairs(config or {}) do
        git(root, "config", pair[1], pair[2])
    end
    return root
end

t.test("remote returns the gerrit remote when the project has one", function()
    local root = repo({ "origin", "gerrit" })
    t.eq(forge.remote(root), "gerrit")
    fs.remove(root, { recursive = true })
end)

t.test("remote returns forge.remote when set", function()
    local config = { { "forge.kind", "plain" }, { "forge.remote", "mirror" } }
    local root = repo({ "origin", "mirror" }, config)
    t.eq(forge.remote(root), "mirror")
    fs.remove(root, { recursive = true })
end)

t.test("remote refuses a remote the project lacks", function()
    local root = repo({})
    local got, err = forge.remote(root)
    t.eq(got, nil)
    t.eq(err, "remote of record origin is not a remote of this repo")
    fs.remove(root, { recursive = true })
end)

t.test("add_change_id appends the trailer hashed from the commit", function()
    local root = repo({})
    fs.write(root .. "/file", "content\n")
    git(root, "add", "file")
    git(root, "commit", "-q", "-m", "subject")
    local before = raw_commit(root)
    local hash = { "git", "-C", root, "hash-object", "-t", "commit", "--stdin" }
    local hashed = devos.capture_input(hash, before)
    t.eq(forge.add_change_id(root), true)
    local body = git(root, "log", "-1", "--format=%B", "HEAD"):gsub("%s+$", "")
    t.eq(body, "subject\n\nChange-Id: I" .. hashed:gsub("%s+$", ""))
    fs.remove(root, { recursive = true })
end)
