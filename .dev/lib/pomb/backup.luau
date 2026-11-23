local M = {}

M.DIR = "/var/lib/pomb-backups"

local function has_dot_dot(path)
    for segment in path:gmatch("[^/]+") do
        if segment == ".." then
            return true
        end
    end
    return false
end

---Resolve a --backup argument to the path of the copy on the host.
---@param arg string a bare file name, or an absolute path under M.DIR
---@return string? path nil when the argument is refused
---@return string? why the refusal, nil when the path resolves
function M.resolve(arg)
    local prefix = M.DIR .. "/"
    local path
    if arg:sub(1, 1) == "/" then
        if arg:sub(1, #prefix) == prefix and #arg > #prefix then
            path = arg
        end
    elseif arg ~= "" then
        path = prefix .. arg
    end
    if not path or has_dot_dot(path) then
        local message = "The --backup argument must be a file name or a path under %s, not %s."
        return nil, message:format(M.DIR, arg)
    end
    return path
end

return M
