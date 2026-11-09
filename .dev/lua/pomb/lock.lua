local devos = require("dev.os")
local fs = require("dev.fs")

---@class pomb.lock
local M = {}

---@class pomb.lock.Holder
---@field batch string the release batch that took the lock
---@field pid integer? process id of the release, nil when the lock text is unparsable

---Name the lock file in the main checkout at `repo`.
---@param repo string absolute path of the main checkout
---@return string path
function M.path(repo)
    return repo .. "/.dev/release.lock"
end

---Read who holds the lock.
---@param repo string absolute path of the main checkout
---@return pomb.lock.Holder? holder nil when no lock is held
function M.holder(repo)
    local text = fs.read(M.path(repo))
    if not text then
        return nil
    end
    local batch, pid = text:match("^batch=(%S*) pid=(%d+)")
    return { batch = batch or "?", pid = tonumber(pid) }
end

---Report whether the process of `holder` is still running.
---@param holder pomb.lock.Holder
---@return boolean alive
function M.alive(holder)
    if holder.pid == nil then
        return false
    end
    return devos.run({ "kill", "-0", tostring(holder.pid) }, { stderr = "null" }) == 0
end

---Write the lock for this process through a rename, so a reader never sees a partial file.
---@param repo string absolute path of the main checkout
---@param batch string the release batch
function M.take(repo, batch)
    local path = M.path(repo)
    local staged = ("%s.%d"):format(path, devos.getpid())
    assert(fs.write(staged, ("batch=%s pid=%d\n"):format(batch, devos.getpid())))
    assert(devos.run({ "mv", "-f", staged, path }) == 0)
end

---Remove the lock when this process holds it.
---@param repo string absolute path of the main checkout
function M.drop(repo)
    local holder = M.holder(repo)
    if holder and holder.pid == devos.getpid() then
        fs.remove(M.path(repo))
    end
end

---Remove the lock whoever holds it.
---@param repo string absolute path of the main checkout
function M.clear(repo)
    fs.remove(M.path(repo))
end

return M
