local M = {}

---Read the package version from the text of a Cargo.toml.
---@param text string Cargo.toml contents
---@return string? version nil when [package] has no version
function M.manifest_version(text)
    local in_package = false
    for line in (text .. "\n"):gmatch("(.-)\n") do
        if line:sub(1, 1) == "[" then
            in_package = line == "[package]"
        end
        local version = in_package and line:match('^version = "(.*)"$')
        if version then
            return version
        end
    end
end

---Read the pomb package version from the text of a Cargo.lock.
---@param text string Cargo.lock contents
---@return string? version nil when the lock has no version for pomb
function M.lock_version(text)
    local found = false
    for line in (text .. "\n"):gmatch("(.-)\n") do
        if line == 'name = "pomb"' then
            found = true
        elseif found then
            local version = line:match('^version = "(.*)"$')
            if version then
                return version
            end
        end
    end
end

return M
