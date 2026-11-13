local devos = require("dev.os")
local fs = require("dev.fs")
local t = require("dev.testing")

---@class pomb.jjenv
local M = {}

---@enum pomb.jjenv.Kind
M.Kind = { GIT = "git", JJ = "jj" }

---Register one test per checkout kind, named after `name` and the kind.
---@param name string test name
---@param body fun(kind: pomb.jjenv.Kind) the test, given the kind it runs in
function M.case(name, body)
    for _, kind in ipairs({ M.Kind.GIT, M.Kind.JJ }) do
        t.test(name .. " in a " .. kind .. " checkout", function()
            body(kind)
        end)
    end
end

local JJ_STUB = [[
[ "$1" = root ] && exit 1
case " $* " in
*" git remote list "*) echo "gerrit x"; exit 0 ;;
*" git root "*) echo '@work@/.git'; exit 0 ;;
esac
echo "$*" >>'@bin@/jj.log'
[ "$1" = --no-pager ] || exit 64
shift
case "$1" in
file) exec '@git@' ls-files --cached --others --exclude-standard ;;
log) ;;
*) exit 0 ;;
esac
rev="$4"
case "$rev" in
'@-') exec '@git@' rev-parse HEAD ;;
'heads(::@- & ::'*')')
    x=${rev#'heads(::@- & ::'}
    x=${x%')'}
    exec '@git@' merge-base HEAD "${x#*@}/${x%@*}"
    ;;
*@*) exec '@git@' rev-parse --verify --quiet "${rev#*@}/${rev%@*}^{commit}" ;;
*) exec '@git@' rev-parse --verify --quiet "$rev^{commit}" ;;
esac
]]

local GIT_LOGGER = [[
echo "$*" >>'@bin@/git.log'
exec '@git@' "$@"
]]

local function script(path, body, subs)
    local text = body:gsub("@(%w+)@", subs)
    assert(fs.write(path, "#!/bin/sh\n" .. text))
    assert(devos.run({ "chmod", "755", path }) == 0)
end

---Turn the git repository at `work` into a jj fixture.
---
---It adds a `.jj` directory, a `jj` in `work/bin` that answers from the git repository, and a
---logging `git` there.
---@param work string absolute path of an existing git repository
function M.install(work)
    local bin = work .. "/bin"
    assert(fs.mkdir(bin, { parents = true }))
    assert(fs.mkdir(work .. "/.jj"))
    local found = assert(devos.capture({ "sh", "-c", "command -v git" }))
    local git = found:gsub("%s+$", "")
    local subs = { bin = bin, git = git, work = work }
    script(bin .. "/jj", JJ_STUB, subs)
    script(bin .. "/git", GIT_LOGGER, subs)
end

return M
