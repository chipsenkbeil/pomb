local devos = require("dev.os")
local fs = require("dev.fs")

---@class pomb.vcs
local M = {}

local function quiet(dir)
    return { cwd = dir, stderr = "null" }
end

---File-listing commands by checkout kind
local STAGE = {
    git = "git ls-files -z --cached --others --exclude-standard",
    jj = "jj --no-pager file list | tr '\\n' '\\0'",
}

local function is_jj(dir)
    return fs.is_dir(dir .. "/.jj")
end

local function jj_revset(rev)
    local remote, name = rev:match("^([^/@]+)/([^@]+)$")
    if remote then
        return name .. "@" .. remote
    end
    return rev
end

local function jj_commit(dir, revset)
    local out, code = devos.capture(
        { "jj", "--no-pager", "log", "--no-graph", "-r", revset, "-T", 'commit_id ++ "\\n"' },
        quiet(dir)
    )
    return code == 0 and out:match("^%x+") or nil
end

---Copy the tracked and new unignored files of the working copy at `root` into `tree`.
---@param root string absolute path of the repository, a jj one when `.jj` exists there
---@param tree string existing directory that receives the files
---@return integer code exit status of the copy
function M.stage(root, tree)
    local lister = is_jj(root) and STAGE.jj or STAGE.git
    return devos.run({
        "bash",
        "-o",
        "pipefail",
        "-c",
        lister
            .. ' | while IFS= read -r -d "" path; do '
            .. 'if [ -e "$path" ] || [ -L "$path" ]; then printf "%s\\0" "$path"; fi; done '
            .. '| tar -cf - --null -T - | tar -xf - -C "$1"',
        "bash",
        tree,
    }, { cwd = root }) or 1
end

---Name the commit the checkout at `dir` sits on.
---
---That is the git HEAD, or the parent of the working-copy commit in jj.
---@param dir string absolute path of the repository or worktree
---@return string? sha full commit id, nil when it cannot be read
function M.head(dir)
    if is_jj(dir) then
        return jj_commit(dir, "@-")
    end
    local out, code = devos.capture({ "git", "-C", dir, "rev-parse", "HEAD" }, quiet())
    return code == 0 and out:match("^%x+") or nil
end

---Find the newest common ancestor of the current commit and `remote_main`.
---@param root string absolute path of the repository
---@param remote_main string remote branch as `remote/main`
---@return string? sha full commit id, nil when there is none
function M.merge_base(root, remote_main)
    if is_jj(root) then
        return jj_commit(root, ("heads(::@- & ::%s)"):format(jj_revset(remote_main)))
    end
    local out, code = devos.capture({ "git", "merge-base", "HEAD", remote_main }, quiet(root))
    return code == 0 and out:match("^%x+") or nil
end

---Resolve `rev` to a full commit id.
---@param root string absolute path of the repository
---@param rev string revision, where `remote/branch` means `branch@remote` in jj
---@return string? sha full commit id, nil when `rev` names no commit
function M.resolve(root, rev)
    if is_jj(root) then
        return jj_commit(root, jj_revset(rev))
    end
    local out, code = devos.capture(
        { "git", "rev-parse", "--verify", "--quiet", rev .. "^{commit}" },
        quiet(root)
    )
    return code == 0 and out:match("^%x+") or nil
end

return M
