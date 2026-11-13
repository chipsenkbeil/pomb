---@class pomb.argv
local M = {}

---Append each of `words` to `argv` in place.
---@param argv string[] command line to extend
---@param words string[] words to append
---@return string[] argv the same list, extended
function M.extend(argv, words)
    for _, word in ipairs(words) do
        argv[#argv + 1] = word
    end
    return argv
end

---Build the `node` command line for the parity script `name`.
---@param ctx dev.Ctx the verb's context
---@param name string script name under `.dev/parity`, without the extension
---@return string[] argv
function M.node(ctx, name)
    return M.extend({ "node", ctx.root .. "/.dev/parity/" .. name .. ".mjs" }, ctx.args)
end

return M
