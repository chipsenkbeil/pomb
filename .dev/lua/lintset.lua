local dev_os = require("dev.os")
local fs = require("dev.fs")
local standalone = require("dev.standalone")
local dev_command = require("dev.command")

---@class lintset.Lint
---@field root string absolute project root
---@field verbose integer 0, 1 or 2, as `ctx.verbose` has it
---@field skip_hidden boolean leave hidden paths out of `files`
---@field failed string[] the name of each failed check, in order
local Lint = {}
Lint.__index = Lint

---@class lintset
local M = {}

local PROSE_RUN = 3
local FAIL_RUN = 7
local CHECK_SPACING = "comment spacing"
local CHECK_RUNS = "comment runs"
---@enum lintset.Kind
local Kind = {
    NONE = "none",
    BLANK = "blank",
    COMMENT = "comment",
    DOC = "doc",
    CODE = "code",
    OPEN = "open",
}
---@enum lintset.Marker
local Marker = { SLASH = "//", DASH = "--", HASH = "#" }
---The terminators that end a shell `case` arm.
local CASE_ARM_ENDS = { [";;"] = true, [";&"] = true, [";;&"] = true }
---The last characters of a line that opens a literal in a `//` file.
local LITERAL_ENDS = { [","] = true, ["("] = true, ["["] = true }

local function lines_of(text)
    local lines = {}
    for line in text:gmatch("([^\n]*)\n") do
        lines[#lines + 1] = line
    end
    local rest = text:match("([^\n]+)$")
    if rest then
        lines[#lines + 1] = rest
    end
    return lines
end

local function starts(text, prefix)
    return text:sub(1, #prefix) == prefix
end

local function classify(body, marker)
    if marker == Marker.SLASH then
        if starts(body, "///") or starts(body, "//!") then
            return Kind.DOC
        end
        if starts(body, Marker.SLASH) then
            return Kind.COMMENT
        end
    elseif marker == Marker.DASH then
        if starts(body, "---") then
            return Kind.DOC
        end
        if starts(body, "--[[") then
            return Kind.COMMENT, not body:find("]]", 1, true)
        end
        if starts(body, Marker.DASH) then
            return Kind.COMMENT
        end
    elseif marker == Marker.HASH then
        if starts(body, "#!") then
            return Kind.DOC
        end
        if starts(body, Marker.HASH) then
            return Kind.COMMENT
        end
    end
    return Kind.CODE
end

---@class lintset.SpacingHit
---@field marker lintset.Marker the comment marker of the file
---@field prev_text string the text of the code line above the comment
---@field same_indent boolean whether the comment sits at the code line's indent
---@field number integer the comment's line number

local function spacing_hit(hit)
    if hit.marker == Marker.SLASH and LITERAL_ENDS[hit.prev_text:sub(-1)] then
        return nil, hit.number
    end
    if hit.marker == Marker.HASH and CASE_ARM_ENDS[hit.prev_text] then
        return hit.same_indent and hit.number or nil
    end
    return hit.number
end

local function after_code_exempt(marker, body)
    local first = body:sub(1, 1)
    if marker == Marker.SLASH then
        return first == "."
    end
    return marker == Marker.DASH and (first == ":" or first == ".")
end

local function opens_in_code(before)
    local plain = before:gsub("%${#[^}]*}", ""):gsub("%$#", ""):gsub("\\.", "")
    local quotes = select(2, plain:gsub("[\"']", ""))
    return quotes % 2 == 0 and not plain:find("^#") and not plain:find("%s#")
end

local function heredoc_openers(body)
    local code = body:gsub("<<<", "")
    local found = {}
    if code:find("((", 1, true) or code:find("$[", 1, true) then
        return found
    end
    local from = 1
    while true do
        local at = code:find("<<", from, true)
        if not at then
            return found
        end
        from = at + 2
        if opens_in_code(code:sub(1, at - 1)) then
            local dash, tail = code:match("^(%-?)%s*(.*)$", from)
            local word = tail:match('^"([^"]+)"')
                or tail:match("^'([^']+)'")
                or tail:match("^\\?([%a_][%w_]*)")
            if word then
                found[#found + 1] = { word = word, strip = dash == "-" }
            end
        end
    end
end

local function heredoc_closes(heredoc, line)
    return (heredoc.strip and line:gsub("^\t+", "") or line) == heredoc.word
end

---Find the line where the heredocs opened on `number` end, or `number` when no opener has a
---terminator later in the text.
local function heredoc_end(lines, number, body)
    local last = number
    for _, heredoc in ipairs(heredoc_openers(body)) do
        local closed
        for at = last + 1, #lines do
            if heredoc_closes(heredoc, lines[at]) then
                closed = at
                break
            end
        end
        if not closed then
            break
        end
        last = closed
    end
    return last
end

---Find the full-line comments that directly follow a line of code.
---
---A blank line, another comment line, an opening brace line or the top of the text before a
---comment passes, and so do the places where a formatter removes the blank line.
---@param text string the file's text
---@param marker lintset.Marker the comment marker
---@return integer[] numbers line numbers of the findings
function M.spacing(text, marker)
    local found = {}
    local kind, prev_indent, prev_text = Kind.NONE, "", ""
    local in_block, pending, literal, body_end = false, nil, nil, 0
    local lines = lines_of(text)
    for number, line in ipairs(lines) do
        local body = line:gsub("^%s+", "")
        local indent = line:sub(1, #line - #body)
        if number <= body_end then
            kind = Kind.CODE
        elseif in_block then
            in_block = not body:find("]]", 1, true)
            kind = Kind.COMMENT
        elseif body == "" then
            kind = Kind.BLANK
        else
            local class, opens = classify(body, marker)
            if class == Kind.DOC then
                kind = Kind.COMMENT
            elseif class == Kind.COMMENT then
                if kind == Kind.CODE and #indent <= #prev_indent then
                    local hit, lit = spacing_hit({
                        marker = marker,
                        prev_text = prev_text,
                        same_indent = indent == prev_indent,
                        number = number,
                    })
                    pending = hit or pending
                    literal = lit or literal
                end
                in_block = opens or false
                kind = Kind.COMMENT
            else
                if pending then
                    if not after_code_exempt(marker, body) then
                        found[#found + 1] = pending
                    end
                    pending = nil
                end
                if literal then
                    if body:find("=>", 1, true) then
                        found[#found + 1] = literal
                    end
                    literal = nil
                end
                kind = body:sub(-1) == "{" and Kind.OPEN or Kind.CODE
                prev_indent, prev_text = indent, body
                if marker == Marker.HASH then
                    body_end = heredoc_end(lines, number, body)
                end
            end
        end
    end
    if pending then
        found[#found + 1] = pending
    end
    table.sort(found)
    return found
end

---The prefixes of the doc and block comments, which are never prose, per marker.
local NOT_PROSE = {
    [Marker.SLASH] = { "///", "//!" },
    [Marker.DASH] = { "---", "--[[" },
    [Marker.HASH] = {},
}

local function is_prose(body, marker)
    if not starts(body, marker) or body:gsub("%s+$", "") == marker then
        return false
    end
    for _, prefix in ipairs(NOT_PROSE[marker]) do
        if starts(body, prefix) then
            return false
        end
    end
    return true
end

---@class lintset.Run
---@field line integer the line the run starts on
---@field count integer how many comment lines it holds

---Find the runs of three or more consecutive full-line comments.
---
---Doc comments and block comments are not counted, a bare marker line ends a run, and a
---`//` run holding a numbered list item passes. A list enumerates distinct cases. A shell
---heredoc body is data, so its lines never make a run.
---@param text string the file's text
---@param marker lintset.Marker the comment marker
---@return lintset.Run[] runs the runs, in line order
function M.runs(text, marker)
    local found = {}
    local run, start, listed, body_end = 0, 0, false, 0
    local function flush()
        if run >= PROSE_RUN and not listed then
            found[#found + 1] = { line = start, count = run }
        end
        run = 0
    end
    local lines = lines_of(text)
    for number, line in ipairs(lines) do
        local body = line:gsub("^%s+", "")
        if number <= body_end then
            flush()
        elseif is_prose(body, marker) then
            if run == 0 then
                start = number
                listed = false
            end
            run = run + 1
            if marker == Marker.SLASH and body:find("^// %d") then
                listed = true
            end
        else
            flush()
            if marker == Marker.HASH then
                body_end = heredoc_end(lines, number, body)
            end
        end
    end
    flush()
    return found
end

local function one_liner(body)
    if not starts(body, Marker.SLASH) then
        return false
    end
    local third = body:sub(3, 3)
    return third ~= "/" and third ~= "!"
end

local function scan_periods(text, fix)
    local lines = lines_of(text)
    local found = {}
    local start, count = 0, 0
    local function flush()
        if count == 1 and lines[start]:match("^[ \t]*(.-)[ \t]*$"):find("[^.]%.$") then
            found[#found + 1] = start
            if fix then
                lines[start] = lines[start]:gsub("%.([ \t]*)$", "%1")
            end
        end
        count = 0
    end
    for number, line in ipairs(lines) do
        local body = line:gsub("^%s+", "")
        if one_liner(body) then
            if count == 0 then
                start = number
            end
            count = count + 1
        else
            flush()
        end
    end
    flush()
    return found, table.concat(lines, "\n") .. (text:sub(-1) == "\n" and "\n" or "")
end

---Find the `//` one-liners that end in a single period.
---
---A run of one `//` line counts. A longer run, a `///` or `//!` doc comment and an ellipsis do not.
---@param text string the file's text
---@return integer[] numbers line numbers of the findings
function M.periods(text)
    return (scan_periods(text, false))
end

---Drop the period from each `//` one-liner that `periods` finds.
---@param text string the file's text
---@return string text the text with every other line as it was
function M.fix_periods(text)
    local _, fixed = scan_periods(text, true)
    return fixed
end

---Append every file to a command prefix.
---@param prefix string[] the command and its leading arguments
---@param files string[] the files to add
---@return string[] argv a new list
function M.with_files(prefix, files)
    local argv = {}
    for _, word in ipairs(prefix) do
        argv[#argv + 1] = word
    end
    for _, file in ipairs(files) do
        argv[#argv + 1] = file
    end
    return argv
end

---Start a lint run for a verb.
---@param ctx dev.Ctx the verb's context
---@return lintset.Lint lint the run, whose `finish` gives the exit code
function M.new(ctx)
    return setmetatable({
        root = ctx.root,
        verbose = ctx.verbose,
        skip_hidden = false,
        failed = {},
    }, Lint)
end

---Rewrite a file with its `//` one-liner periods dropped.
---@param root string the project root
---@param file string the path relative to the root
---@return true? ok true on success
---@return string? message why it failed
function M.fix_comment_periods(root, file)
    local path = fs.join(root, file)
    local text, err = fs.read(path)
    if not text then
        return nil, err
    end
    local fixed = M.fix_periods(text)
    if fixed == text then
        return true
    end
    return fs.write(path, fixed)
end

---Print the name of the check that starts.
---@param name string the check's name
function Lint:step(name)
    dev_command.say("> " .. name)
end

---Add a failed check's name to the list unless it is already there.
---@param failed string[] the failed checks, in the order each first failed
---@param name string the check's name
local function record(failed, name)
    for _, seen in ipairs(failed) do
        if seen == name then
            return
        end
    end
    failed[#failed + 1] = name
end

---Record a failure found by an inline check.
---@param name string the check's name
---@param message string what is wrong, written to standard error
function Lint:fail(name, message)
    dev_command.note(message)
    record(self.failed, name)
end

---Run a command in the project root and record a failure without stopping.
---@param name string the check's name
---@param argv string[] the command and its arguments
function Lint:check(name, argv)
    if self.verbose >= 1 then
        dev_command.say("> " .. name .. ": " .. table.concat(argv, " "))
    else
        self:step(name)
    end
    if dev_os.run(argv, { cwd = self.root }) ~= 0 then
        record(self.failed, name)
    end
end

---Skip the check with a note that its tool is missing, or fail it under CI, which never skips one.
---@param binary string the missing tool
---@param install string how to install it
---@param name string the check it would have run
function Lint:missing(binary, install, name)
    local ci = os.getenv("CI")
    local strict = ci and ci ~= ""
    local text = ("Tool %s not found. The %s check %s Install it with %s."):format(
        binary,
        name,
        strict and "cannot run. CI never skips a check." or "is skipped.",
        install
    )
    if strict then
        self:fail(name, text)
        return
    end
    dev_command.note(text)
end

---Say whether a tool exists, and report it as missing when it does not.
---@param binary string the tool
---@param install string how to install it
---@param name string? the check it serves, the binary when nil
---@return boolean found whether the tool is on PATH
function Lint:have(binary, install, name)
    if dev_os.which(binary) then
        return true
    end
    self:missing(binary, install, name or binary)
    return false
end

local function ignore_globs(root)
    local text = fs.read(root .. "/.dev/lint-ignore")
    local out = {}
    for _, line in ipairs(text and lines_of(text) or {}) do
        if line ~= "" and line:sub(1, 1) ~= "#" then
            out[#out + 1] = line
        end
    end
    return out
end

local function is_hidden(file)
    return file:sub(1, 1) == "." or file:find("/.", 1, true) ~= nil
end

local function visible(lint, file, query)
    return query.hidden or not (lint.skip_hidden and is_hidden(file))
end

local function files_jj(lint, query)
    local globs, ignored = query.globs, query.ignored
    local root = lint.root
    if not (fs.exists(root .. "/.jj") and dev_os.which("jj")) then
        return nil
    end
    local include = {}
    for _, glob in ipairs(globs) do
        include[#include + 1] = 'glob:"**/' .. glob .. '"'
    end
    local fileset = "(" .. table.concat(include, " | ") .. ")"
    for _, glob in ipairs(ignored) do
        fileset = fileset .. ' ~ glob:"' .. glob .. '"'
    end
    fileset = fileset .. ' ~ glob:"**/target/**"'
    local out, code = dev_os.capture(
        { "jj", "file", "list", fileset },
        { cwd = root, stderr = "null" }
    )
    if code ~= 0 then
        dev_command.note(
            "The jj file list command failed with exit "
                .. code
                .. ". A bad glob in .dev/lint-ignore is the usual cause."
                .. " Lint falls back to git or rg."
        )
        return nil
    end
    local files = {}
    for file in out:gmatch("[^\n]+") do
        if visible(lint, file, query) then
            files[#files + 1] = file
        end
    end
    return files
end

local function files_git(lint, query)
    local globs, ignored = query.globs, query.ignored
    local root = lint.root
    local _, inside = dev_os.capture(
        { "git", "rev-parse", "--is-inside-work-tree" },
        { cwd = root, stderr = "null" }
    )
    if inside ~= 0 then
        return nil
    end
    local argv = { "git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", "--" }
    for _, glob in ipairs(globs) do
        argv[#argv + 1] = ":(glob)**/" .. glob
    end
    for _, glob in ipairs(ignored) do
        argv[#argv + 1] = ":(exclude,glob)" .. glob
    end
    argv[#argv + 1] = ":(exclude,glob)**/target/**"
    local out = dev_os.capture(argv, { cwd = root })
    local files = {}
    for file in out:gmatch("([^%z]+)") do
        if
            file:sub(-1) ~= "/"
            and visible(lint, file, query)
            and fs.is_file(fs.join(root, file))
        then
            files[#files + 1] = file
        end
    end
    return files
end

local function files_rg(lint, query)
    local globs, ignored = query.globs, query.ignored
    local argv = { "rg", "--files", "--no-require-git" }
    if lint.skip_hidden and not query.hidden then
        argv[#argv + 1] = "--no-hidden"
    else
        for _, word in ipairs({ "--hidden", "-g", "!.git", "-g", "!.jj" }) do
            argv[#argv + 1] = word
        end
    end
    for _, glob in ipairs(ignored) do
        argv[#argv + 1] = "-g"
        argv[#argv + 1] = "!" .. glob
    end
    for _, word in ipairs({ "-g", "!target", "." }) do
        argv[#argv + 1] = word
    end
    local patterns = {}
    for _, glob in ipairs(globs) do
        local lua_pattern = glob:gsub("[%^%$%(%)%%%.%[%]%+%-]", "%%%0")
        lua_pattern = lua_pattern:gsub("%*", "[^/]*"):gsub("%?", "[^/]")
        patterns[#patterns + 1] = "^" .. lua_pattern .. "$"
    end
    local out = dev_os.capture(argv, { cwd = lint.root })
    local files = {}
    for file in out:gmatch("[^\n]+") do
        local name = file:match("[^/]*$")
        for _, pattern in ipairs(patterns) do
            if name:find(pattern) then
                files[#files + 1] = file:gsub("^%./", "")
                break
            end
        end
    end
    return files
end

---List the project's files matching the globs, never under a target directory.
---
---In a jj checkout that is what `jj file list` shows. Inside a git work tree it is what git tracks
---or would track, so a nested checkout is never walked. Elsewhere it is a ripgrep walk. A project
---lists globs relative to its root in `.dev/lint-ignore`, one per line, to leave those out, and
---`skip_hidden` drops hidden paths.
---@param globs string[] file name globs such as `*.rs`
---@param opts? { hidden?: boolean } `hidden` lists hidden paths even when `skip_hidden` is set
---@return string[] files paths relative to the root
function Lint:files(globs, opts)
    local query = {
        globs = globs,
        ignored = ignore_globs(self.root),
        hidden = opts and opts.hidden,
    }
    return files_jj(self, query) or files_git(self, query) or files_rg(self, query)
end

---List the Lua files under the project's `.dev` and `.ci` directories, and no other Lua.
---
---The list holds them whether or not hidden paths are skipped, and `.dev/lint-ignore` still leaves
---paths out.
---@return string[] files paths relative to the root
function Lint:dev_lua()
    local files = {}
    for _, file in ipairs(self:files({ "*.lua" }, { hidden = true })) do
        if file:find("^%.dev/") or file:find("^%.ci/") then
            files[#files + 1] = file
        end
    end
    return files
end

---List every Lua file the lint and fmt verbs check: the visible Lua, then the `.dev` and `.ci` Lua.
---@return string[] files paths relative to the root
function Lint:all_lua()
    local files = self:files({ "*.lua" })
    for _, file in ipairs(self:dev_lua()) do
        files[#files + 1] = file
    end
    return files
end

---@class lintset.Hit
---@field file string the path as ripgrep printed it
---@field line integer the line number
---@field text string the matching line

---Search files with ripgrep.
---@param pattern string the regular expression
---@param files string[] paths relative to the root
---@return lintset.Hit[] hits the matches, in ripgrep's order
function Lint:grep(pattern, files)
    if #files == 0 then
        return {}
    end
    local argv = M.with_files({
        "rg",
        "--no-heading",
        "-n",
        "--with-filename",
        "--field-match-separator",
        "::",
        pattern,
    }, files)
    local out = dev_os.capture(argv, { cwd = self.root })
    local hits = {}
    for row in out:gmatch("[^\n]+") do
        local file, number, text = row:match("^(.-)::(%d+)::(.*)$")
        if file then
            hits[#hits + 1] = { file = file, line = tonumber(number), text = text }
        end
    end
    return hits
end

---Read a project file, or fail the check naming the file when it cannot be read.
---@param lint lintset.Lint the lint run
---@param name string the check that needs the file
---@param file string the path relative to the root
---@return string? text the file's text, nil after the failure
local function read_or_fail(lint, name, file)
    local text, err = fs.read(fs.join(lint.root, file))
    if not text then
        lint:fail(name, file .. ": cannot read the file: " .. tostring(err))
    end
    return text
end

---Fail on each full-line comment that directly follows code.
---@param file string the path relative to the root
---@param marker lintset.Marker the comment marker
function Lint:comment_spacing(file, marker)
    local text = read_or_fail(self, CHECK_SPACING, file)
    if not text then
        return
    end
    for _, number in ipairs(M.spacing(text, marker)) do
        self:fail(
            CHECK_SPACING,
            file
                .. ":"
                .. number
                .. ": full-line comment directly after code, add a blank line before it"
        )
    end
end

---Fail on each run of seven or more consecutive full-line comments and warn from three.
---@param file string the path relative to the root
---@param marker lintset.Marker the comment marker
function Lint:comment_runs(file, marker)
    local text = read_or_fail(self, CHECK_RUNS, file)
    if not text then
        return
    end
    for _, run in ipairs(M.runs(text, marker)) do
        local message = file
            .. ":"
            .. run.line
            .. ": "
            .. run.count
            .. " consecutive comment lines, restructure the code or shrink the comment"
        if run.count >= FAIL_RUN then
            self:fail(CHECK_RUNS, message)
        else
            dev_command.note(message)
        end
    end
end

---Fail on non-helper shell outside .dev/standalone and on bad standalone headers.
function Lint:standalone()
    self:step("standalone shell")
    for _, line in ipairs(standalone.problems(self.root)) do
        self:fail("standalone shell", line)
    end
end

---Give the exit code: 0 when every check passed, else 1 after naming the failed checks.
---@return integer code the exit code
function Lint:finish()
    if #self.failed == 0 then
        return 0
    end
    dev_command.note("These checks failed: " .. table.concat(self.failed, ", ") .. ".")
    return 1
end

return M
