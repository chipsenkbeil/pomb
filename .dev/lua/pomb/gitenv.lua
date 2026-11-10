local devos = require("dev.os")

---@class pomb.gitenv
local M = {}

---@type table<string, string>
M.env = {
    GIT_CONFIG_GLOBAL = "/dev/null",
    GIT_CONFIG_NOSYSTEM = "1",
    GIT_AUTHOR_NAME = "Test",
    GIT_AUTHOR_EMAIL = "test@example.com",
    GIT_COMMITTER_NAME = "Test",
    GIT_COMMITTER_EMAIL = "test@example.com",
}

---Run git in `dir` and return its output with trailing whitespace trimmed.
---@param dir string absolute path of the repository
---@param ... string git arguments
---@return string output
function M.git(dir, ...)
    local out, code = devos.capture({ "git", "-C", dir, ... })
    assert(code == 0, table.concat({ ... }, " "))
    return (out:gsub("%s+$", ""))
end

return M
