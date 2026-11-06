local devos = require("dev.os")
local fs = require("dev.fs")

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
local KIND = { NONE = "none", BLANK = "blank", COMMENT = "comment", CODE = "code", OPEN = "open" }
local CASE_ARM_ENDS = { [";;"] = true, [";&"] = true, [";;&"] = true }
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
    if marker == "//" then
        if starts(body, "///") or starts(body, "//!") then
            return "doc"
        end
        if starts(body, "//") then
            return "comment"
        end
    elseif marker == "--" then
        if starts(body, "---") then
            return "doc"
        end
        if starts(body, "--[[") then
            return "comment", not body:find("]]", 1, true)
        end
        if starts(body, "--") then
            return "comment"
        end
    elseif marker == "#" then
        if starts(body, "#!") then
            return "doc"
        end
        if starts(body, "#") then
            return "comment"
        end
    end
    return "code"
end

local function spacing_hit(marker, prev_text, same_indent, number)
    if marker == "//" and LITERAL_ENDS[prev_text:sub(-1)] then
        return nil, number
    end
    if marker == "#" and CASE_ARM_ENDS[prev_text] then
        return same_indent and number or nil
    end
    return number
end

local function after_code_exempt(marker, body)
    local first = body:sub(1, 1)
    if marker == "//" then
        return first == "."
    end
    return marker == "--" and (first == ":" or first == ".")
end

---Find the full-line comments that directly follow a line of code.
---
---A blank line, another comment line, an opening brace line or the top of the text before a
---comment passes, and so do the places where a formatter removes the blank line.
---@param text string the file's text
---@param marker string `//`, `--` or `#`
---@return integer[] numbers line numbers of the findings
function M.spacing(text, marker)
    local found = {}
    local kind, prev_indent, prev_text = KIND.NONE, "", ""
    local in_block, pending, literal = false, nil, nil
    for number, line in ipairs(lines_of(text)) do
        local body = line:gsub("^%s+", "")
        local indent = line:sub(1, #line - #body)
        if in_block then
            in_block = not body:find("]]", 1, true)
            kind = KIND.COMMENT
        elseif body == "" then
            kind = KIND.BLANK
        else
            local class, opens = classify(body, marker)
            if class == "doc" then
                kind = KIND.COMMENT
            elseif class == "comment" then
                if kind == KIND.CODE and #indent <= #prev_indent then
                    local hit, lit = spacing_hit(marker, prev_text, indent == prev_indent, number)
                    pending = hit or pending
                    literal = lit or literal
                end
                in_block = opens or false
                kind = KIND.COMMENT
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
                kind = body:sub(-1) == "{" and KIND.OPEN or KIND.CODE
                prev_indent, prev_text = indent, body
            end
        end
    end
    if pending then
        found[#found + 1] = pending
    end
    table.sort(found)
    return found
end

local NOT_PROSE = { ["//"] = { "///", "//!" }, ["--"] = { "---", "--[[" }, ["#"] = {} }

local function is_prose(body, marker)
    if not starts(body, marker) then
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
---Doc comments and block comments are not counted, and a `//` run holding a numbered list item
---passes, since a list enumerates distinct cases.
---@param text string the file's text
---@param marker string `//`, `--` or `#`
---@return lintset.Run[] runs the runs, in line order
function M.runs(text, marker)
    local found = {}
    local run, start, listed = 0, 0, false
    local function flush()
        if run >= PROSE_RUN and not listed then
            found[#found + 1] = { line = start, count = run }
        end
        run = 0
    end
    for number, line in ipairs(lines_of(text)) do
        local body = line:gsub("^%s+", "")
        if is_prose(body, marker) then
            if run == 0 then
                start = number
                listed = false
            end
            run = run + 1
            if marker == "//" and body:find("^// %d") then
                listed = true
            end
        else
            flush()
        end
    end
    flush()
    return found
end

local function one_liner(body)
    if not starts(body, "//") then
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
    print("> " .. name)
end

---Record a failure found by an inline check.
---@param name string the check's name
---@param message string what is wrong, written to standard error
function Lint:fail(name, message)
    io.stderr:write(message, "\n")
    self.failed[#self.failed + 1] = name
end

---Run a command in the project root and record a failure without stopping.
---@param name string the check's name
---@param argv string[] the command and its arguments
function Lint:check(name, argv)
    if self.verbose >= 1 then
        print("> " .. name .. ": " .. table.concat(argv, " "))
    else
        self:step(name)
    end
    if devos.run(argv, { cwd = self.root }) ~= 0 then
        self.failed[#self.failed + 1] = name
    end
end

---Warn that a tool is missing, or fail the check under CI, which never skips one.
---@param binary string the missing tool
---@param install string how to install it
---@param name string the check it would have run
function Lint:missing(binary, install, name)
    local ci = os.getenv("CI")
    if ci and ci ~= "" then
        self:fail(
            name,
            binary
                .. " not found, so the "
                .. name
                .. " check cannot run, and CI never skips a check. Install with: "
                .. install
        )
        return
    end
    io.stderr:write(
        "WARNING: ",
        binary,
        " not found, skipping the ",
        name,
        " check. Install with: ",
        install,
        "\n"
    )
end

---Say whether a tool exists, and report it as missing when it does not.
---@param binary string the tool
---@param install string how to install it
---@param name string? the check it serves, the binary when nil
---@return boolean found whether the tool is on PATH
function Lint:have(binary, install, name)
    if devos.which(binary) then
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

---List the project's files matching the globs, never under a target directory.
---
---Inside a git work tree that is what git tracks or would track, so a nested checkout is never
---walked. Elsewhere it is a ripgrep walk. A project lists globs relative to its root in
---`.dev/lint-ignore`, one per line, to leave those out, and `skip_hidden` drops hidden paths.
---@param globs string[] file name globs such as `*.rs`
---@return string[] files paths relative to the root
function Lint:files(globs)
    local root = self.root
    local ignored = ignore_globs(root)
    local _, inside = devos.capture(
        { "git", "rev-parse", "--is-inside-work-tree" },
        { cwd = root, stderr = "null" }
    )
    local files = {}
    if inside == 0 then
        local argv = { "git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", "--" }
        for _, glob in ipairs(globs) do
            argv[#argv + 1] = ":(glob)**/" .. glob
        end
        for _, glob in ipairs(ignored) do
            argv[#argv + 1] = ":(exclude,glob)" .. glob
        end
        argv[#argv + 1] = ":(exclude,glob)**/target/**"
        local out = devos.capture(argv, { cwd = root })
        for file in out:gmatch("([^%z]+)") do
            local skip = file:sub(-1) == "/" or (self.skip_hidden and is_hidden(file))
            if not skip and fs.is_file(fs.join(root, file)) then
                files[#files + 1] = file
            end
        end
        return files
    end
    local argv = { "rg", "--files" }
    if self.skip_hidden then
        argv[#argv + 1] = "--no-hidden"
    else
        for _, word in ipairs({ "--hidden", "-g", "!.git", "-g", "!.jj" }) do
            argv[#argv + 1] = word
        end
    end
    for _, glob in ipairs(globs) do
        argv[#argv + 1] = "-g"
        argv[#argv + 1] = glob
    end
    for _, glob in ipairs(ignored) do
        argv[#argv + 1] = "-g"
        argv[#argv + 1] = "!" .. glob
    end
    for _, word in ipairs({ "-g", "!target", "." }) do
        argv[#argv + 1] = word
    end
    local out = devos.capture(argv, { cwd = root })
    for file in out:gmatch("[^\n]+") do
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
    local out = devos.capture(argv, { cwd = self.root })
    local hits = {}
    for row in out:gmatch("[^\n]+") do
        local file, number, text = row:match("^(.-)::(%d+)::(.*)$")
        if file then
            hits[#hits + 1] = { file = file, line = tonumber(number), text = text }
        end
    end
    return hits
end

---Fail on each full-line comment that directly follows code.
---@param file string the path relative to the root
---@param marker string `//`, `--` or `#`
function Lint:comment_spacing(file, marker)
    local text = fs.read(fs.join(self.root, file)) or ""
    for _, number in ipairs(M.spacing(text, marker)) do
        self:fail(
            "comment spacing",
            file .. ":" .. number .. ": full-line comment directly after code, add a blank line before it"
        )
    end
end

---Fail on each run of three or more consecutive full-line comments.
---@param file string the path relative to the root
---@param marker string `//`, `--` or `#`
function Lint:comment_runs(file, marker)
    local text = fs.read(fs.join(self.root, file)) or ""
    for _, run in ipairs(M.runs(text, marker)) do
        self:fail(
            "comment runs",
            file
                .. ":"
                .. run.line
                .. ": "
                .. run.count
                .. " consecutive comment lines, restructure the code or shrink the comment"
        )
    end
end

---Give the exit code: 0 when every check passed, else 1 after naming the failed checks.
---@return integer code the exit code
function Lint:finish()
    if #self.failed == 0 then
        return 0
    end
    io.stderr:write("\nFAILED: ", table.concat(self.failed, " "), "\n")
    return 1
end

return M
