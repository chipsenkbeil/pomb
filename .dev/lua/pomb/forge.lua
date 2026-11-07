local devos = require("dev.os")
local forge = require("dev.forge")

---@class pomb.forge
local M = {}

local QUIET = { stderr = "null" }
local MISSING = "^the remote of record (%S+) is not a remote of this project"

---Find the remote of record of the project at `root`.
---@param root string absolute path of the project root
---@return string? remote name of the remote of record, nil on failure
---@return string? err why there is none
function M.remote(root)
    local found, err = forge.resolve(root)
    if found then
        return found.remote
    end
    local name = err and err:match(MISSING)
    if name then
        return nil, ("remote of record %s is not a remote of this repo"):format(name)
    end
    return nil, err
end

local function git(root, argv)
    local words = { "git", "-C", root }
    for _, word in ipairs(argv) do
        words[#words + 1] = word
    end
    return words
end

---Amend HEAD with a Change-Id trailer hashed from the commit, as Gerrit's commit-msg hook does.
---@param root string absolute path of the repository
---@return boolean ok true when the amend succeeded
function M.add_change_id(root)
    local commit, code = devos.capture(git(root, { "cat-file", "commit", "HEAD" }), QUIET)
    if code ~= 0 then
        return false
    end
    local hash = git(root, { "hash-object", "-t", "commit", "--stdin" })
    local id
    id, code = devos.capture_input(hash, commit, QUIET)
    if code ~= 0 then
        return false
    end
    local body
    body, code = devos.capture(git(root, { "log", "-1", "--format=%B", "HEAD" }), QUIET)
    if code ~= 0 then
        return false
    end
    local message = body .. "\nChange-Id: I" .. id:gsub("%s+$", "") .. "\n"
    local amend = git(root, { "commit", "-q", "--amend", "-F", "-" })
    local _, amended = devos.capture_input(amend, message, QUIET)
    return amended == 0
end

return M
