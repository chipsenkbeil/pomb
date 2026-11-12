local devos = require("dev.os")
local fs = require("dev.fs")
local verb = require("dev.verb")

---@class pomb.site
---@field HOST_ENTRY string pass entry holding the box user@host
---@field ADMIN_IP_ENTRY string pass entry holding the admin address
---@field CHANNEL table<string, string> release channel words by name
---@field USAGE integer exit code of a usage error
local M = {}

M.HOST_ENTRY = "Agents/Linode/pomb.app-host"
M.ADMIN_IP_ENTRY = "Agents/Linode/pomb.app-admin-ip"
M.CHANNEL = { STABLE = "stable", NIGHTLY = "nightly" }
M.USAGE = 2

local cached_host

local function non_empty(value)
    return value ~= nil and value ~= ""
end

---Append `words` to `list`.
---@param list string[] list to extend
---@param words string[] words to add
---@return string[] list the same list
function M.append(list, words)
    for _, word in ipairs(words) do
        list[#list + 1] = word
    end
    return list
end

---Tell whether `name` is a release channel.
---@param name string word to test
---@return boolean channel true for stable and nightly
function M.is_channel(name)
    return name == M.CHANNEL.STABLE or name == M.CHANNEL.NIGHTLY
end

---Build the ssh options that select the key and stop prompts.
---@return string[] opts the option words, with `POMB_KEY` or `~/.ssh/pomb_app` as the key
function M.ssh_opts()
    local key = os.getenv("POMB_KEY")
    if not non_empty(key) then
        key = os.getenv("HOME") .. "/.ssh/pomb_app"
    end
    return {
        "-i",
        key,
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=15",
        "-o",
        "LogLevel=ERROR",
    }
end

---Read the first line of a password store entry, and exit 1 when it is missing or empty.
---@param entry string pass entry name
---@return string line the first line of the entry
function M.pass_line(entry)
    local out, code = devos.capture({ "pass", "show", entry }, { stderr = "null" })
    local value = out:gsub("\n+$", "")
    if code ~= 0 or value == "" then
        verb.die("site: the password store entry " .. entry .. " is missing or empty")
    end
    return value:match("^[^\n]*")
end

---Name the box as `user@host`, from `POMB_HOST` or the password store.
---
---Exits 1 when the entry is missing, empty or malformed.
---@return string host the ssh destination
function M.host()
    local host = os.getenv("POMB_HOST")
    if non_empty(host) then
        return host
    end
    if cached_host then
        return cached_host
    end
    host = M.pass_line(M.HOST_ENTRY)
    if not host:match("^[%w_.-]+@[^@%s]+$") then
        verb.die("site: the password store entry " .. M.HOST_ENTRY .. " is not user@host")
    end
    cached_host = host
    return host
end

---Build an ssh command line with the shared options.
---@param host string ssh destination
---@param words string[] remote command words
---@return string[] argv the command line
function M.ssh_argv(host, words)
    local argv = M.append({ "ssh" }, M.ssh_opts())
    argv[#argv + 1] = host
    return M.append(argv, words)
end

---Run a command on the box.
---@param words string[] remote command words
---@return integer code ssh exit status, 1 when it had none
function M.ssh(words)
    return devos.run(M.ssh_argv(M.host(), words)) or 1
end

---Run a shell script on the box through its standard input.
---@param script string the script text
---@param words string[] remote command words that read it
---@return integer code ssh exit status, 1 when it had none
function M.ssh_script(script, words)
    local argv = M.ssh_argv(M.host(), words)
    local file, why = fs.mktemp({ prefix = "pomb-script" })
    if not file then
        verb.die(why)
    end
    local ok, code, message = pcall(function()
        local written, why_not = fs.write(file, script)
        if not written then
            return nil, why_not
        end
        return devos.run(
            M.append({ "sh", "-c", 'file=$1; shift; exec "$@" <"$file"', "sh", file }, argv)
        ) or 1
    end)
    fs.remove(file)
    if not ok then
        error(code, 0)
    end
    if not code then
        verb.die(message)
    end
    return code
end

---Name the folder of the site verbs' box files.
---@param ctx dev.Ctx the verb's context
---@return string dir the absolute path of `.dev/site`
function M.dir(ctx)
    return ctx.root .. "/.dev/site"
end

---Read a remote script from the site folder, and exit 1 when it cannot be read.
---@param ctx dev.Ctx the verb's context
---@param name string script name without its `.sh` suffix
---@return string script the script text, sent to the box unchanged
function M.read_script(ctx, name)
    local text, why = fs.read(M.dir(ctx) .. "/" .. name .. ".sh")
    if not text then
        verb.die(why)
    end
    return text
end

return M
