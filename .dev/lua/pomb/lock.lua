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

---@class pomb.lock.LandHolder
---@field card string the card whose land took the lock
---@field pid integer? process id of the land, nil when the lock text is unparsable

---Hold one land at a time.
M.land = {}

---Name the land lock file in the main checkout at `repo`.
---@param repo string absolute path of the main checkout
---@return string path
function M.land.path(repo)
    return repo .. "/.dev/land.lock"
end

---Read who holds the land lock.
---@param repo string absolute path of the main checkout
---@return pomb.lock.LandHolder? holder nil when no lock is held
function M.land.holder(repo)
    local text = fs.read(M.land.path(repo))
    if not text then
        return nil
    end
    local card, pid = text:match("^card=(%S*) pid=(%d+)")
    return { card = card or "?", pid = tonumber(pid) }
end

---Report whether the process of `holder` is still running.
---@param holder pomb.lock.LandHolder
---@return boolean alive
function M.land.alive(holder)
    return M.alive(holder)
end

---Take the land lock for this process when no other land holds it.
---@param repo string absolute path of the main checkout
---@param card string the card being landed
---@return boolean taken false when another lock file already exists
function M.land.take(repo, card)
    local path = M.land.path(repo)
    local staged = ("%s.%d"):format(path, devos.getpid())
    assert(fs.write(staged, ("card=%s pid=%d\n"):format(card, devos.getpid())))
    local linked = devos.run({ "ln", staged, path }, { stderr = "null" }) == 0
    fs.remove(staged)
    return linked
end

---Remove the land lock when this process holds it.
---@param repo string absolute path of the main checkout
function M.land.drop(repo)
    local holder = M.land.holder(repo)
    if holder and holder.pid == devos.getpid() then
        fs.remove(M.land.path(repo))
    end
end

---Remove the land lock when it still names the process of `stale`.
---@param repo string absolute path of the main checkout
---@param stale pomb.lock.LandHolder the holder read earlier
function M.land.clear(repo, stale)
    local holder = M.land.holder(repo)
    if holder and holder.pid == stale.pid and holder.card == stale.card then
        fs.remove(M.land.path(repo))
    end
end

return M
