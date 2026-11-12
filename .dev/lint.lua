--[[
Run the Rust lint set over the project.
Cargo fmt, clippy, ast-grep, cargo-sort and cargo-deny run first, then the manifest and source
checks. Every failed check is named at the end, and the exit code is non-zero when any failed.
]]
local dev_os = require("dev.os")
local fs = require("dev.fs")
local lintset = require("lintset")
local verb = require("dev.verb")

local EDITION = "2024"
local CANONICAL = { "Copy", "Clone", "Debug", "Default", "PartialEq", "Eq", "PartialOrd", "Ord", "Hash" }
local DEPENDENCY_TABLES = [=[[.dependencies, .["dev-dependencies"], .["build-dependencies"],
     .workspace.dependencies]]=]
local UNPINNED = DEPENDENCY_TABLES
    .. [=[ | .[] | select(. != null) | to_entries | .[]
    | select(.value | tag == "!!map") | select(.value.path != null)
    | select((.value.version // "") | test("^=") | not) | .key]=]
local BANNED = [=[[.dependencies, .["dev-dependencies"], .["build-dependencies"],
     .workspace.dependencies, ((.target // {}) | .[] | .dependencies)]
    | .[] | select(. != null) | keys | .[]
    | select(test("^(once_cell|lazy_static|async-trait)$"))]=]
local THISERROR = [=[[.dependencies, ((.target // {}) | .[] | .dependencies)]
    | .[] | select(. != null) | keys | .[]
    | select(. == "thiserror")]=]
local ALLOWED_UNSAFE = [=[.workspace.metadata.style["allow-unsafe"] // [] | .[]
    | sub("^\./", "") | sub("/$", "")]=]

local ctx = ...
verb.no_args(ctx)
local lint = lintset.new(ctx)
local root = ctx.root
local rs_files = lint:files({ "*.rs" })

local function read(file)
    return fs.read(fs.join(root, file)) or ""
end

local function pattern(name, regex, message)
    if #rs_files == 0 then
        return
    end
    lint:step(name)
    for _, hit in ipairs(lint:grep(regex, rs_files)) do
        lint:fail(name, hit.file .. ":" .. hit.line .. ": " .. message)
    end
end

local function yq(...)
    local out = dev_os.capture({ "yq", ... }, { cwd = root })
    return (out:gsub("\n+$", ""))
end

local function names(...)
    local out = {}
    for name in yq(...):gmatch("[^\n]+") do
        out[#out + 1] = (name:gsub('"', ""))
    end
    return out
end

local function field(file, path)
    return yq("-r", "-p", "toml", "-o", "json", path .. ' // ""', file)
end

local function published(manifest)
    local value
    if yq("-p", "toml", "-o", "yaml", ".package.publish | kind", manifest) == "map" then
        value = yq("-p", "toml", "-o", "json", "-I0", ".workspace.package.publish", "Cargo.toml")
    else
        value = yq("-p", "toml", "-o", "json", "-I0", ".package.publish", manifest)
    end
    return value ~= "false" and value ~= "[]"
end

local function resolve(manifest, key)
    local path = '.package["' .. key .. '"]'
    if yq("-p", "toml", "-o", "yaml", path .. " | kind", manifest) == "map" then
        return field("Cargo.toml", '.workspace.package["' .. key .. '"]')
    end
    return field(manifest, path)
end

local function check_edition(manifest)
    local edition = resolve(manifest, "edition")
    if edition ~= EDITION then
        local shown = edition == "" and "unset" or edition
        lint:fail("manifests", manifest .. ": edition is '" .. shown .. "', want " .. EDITION)
    end
end

local function check_rust_version(manifest)
    local rust_version = resolve(manifest, "rust-version")
    if not published(manifest) then
        if fs.is_file(root .. "/rust-toolchain.toml") and rust_version ~= "" then
            lint:fail(
                "manifests",
                manifest
                    .. ": declares rust-version "
                    .. rust_version
                    .. ", but the crate sets publish = false, remove it and let rust-toolchain.toml pin the compiler"
            )
        end
    elseif rust_version == "" then
        lint:fail(
            "manifests",
            manifest
                .. ": declares no rust-version, but the crate is published, and it does not set publish = false"
        )
    end
end

local function check_manifest(manifest)
    for _, dep in ipairs(names("-p", "toml", "-o", "json", UNPINNED, manifest)) do
        lint:fail(
            "manifests",
            manifest
                .. ": path dependency '"
                .. dep
                .. "' needs an exact version, such as version = \"=1.2.3\""
        )
    end
    for _, dep in ipairs(names("-p", "toml", "-o", "json", BANNED, manifest)) do
        if dep == "async-trait" then
            lint:fail(
                "manifests",
                manifest .. ": depends on async-trait, write async trait methods as native async fn"
            )
        else
            lint:fail("manifests", manifest .. ": depends on " .. dep .. ", use std::sync::LazyLock")
        end
    end
    local thiserror = #names("-p", "toml", "-o", "json", THISERROR, manifest) > 0
    local has_lib = fs.is_file(fs.join(root, fs.dirname(manifest), "src/lib.rs"))
    if thiserror and has_lib and published(manifest) then
        lint:fail(
            "thiserror",
            manifest
                .. ": a library crate depends on thiserror, write the error type by hand with Display and Error impls"
        )
    end
    if yq("-p", "toml", "-o", "yaml", ".package | kind", manifest) ~= "map" then
        local edition = field(manifest, ".workspace.package.edition")
        if edition ~= "" and edition ~= EDITION then
            lint:fail("manifests", manifest .. ": edition is " .. edition .. ", want " .. EDITION)
        end
        return
    end
    check_edition(manifest)
    check_rust_version(manifest)
end

local function mentions_debug_assertions()
    for _, file in ipairs(rs_files) do
        if read(file):find("debug_assertions", 1, true) then
            return true
        end
    end
    return false
end

local function derive_in_order(text)
    local list = text:match("derive%(([^)]*)")
    local last = 0
    for trait in (list or ""):gmatch("[^,]+") do
        local name = trait:gsub("%s", ""):match("[^:]*$")
        for index, canonical in ipairs(CANONICAL) do
            if canonical == name then
                if index < last then
                    return false
                end
                last = index
            end
        end
    end
    return true
end

local function check_derive_order()
    lint:step("derive order")
    for _, hit in ipairs(lint:grep([[derive\(]], rs_files)) do
        if not derive_in_order(hit.text) then
            lint:fail(
                "derive order",
                hit.file
                    .. ":"
                    .. hit.line
                    .. ": derive list out of order, want Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash:"
                    .. hit.text
            )
        end
    end
end

local function module_docs_run(file, start, count)
    if count > 0 and count < 4 then
        lint:fail(
            "module docs",
            file
                .. ":"
                .. start
                .. ": //! block is "
                .. count
                .. " lines, make it a 4+ line essay with a # section for an API module, or replace it with one /// line at the pub mod declaration"
        )
    end
end

local function check_module_docs(file)
    local name = fs.basename(file)
    if name == "lib.rs" or name == "main.rs" then
        return
    end
    local start, count, prev = 0, 0, 0
    local number = 0
    for line in (read(file) .. "\n"):gmatch("([^\n]*)\n") do
        number = number + 1
        if line:find("^%s*//!") then
            if count > 0 and number == prev + 1 then
                count = count + 1
            else
                module_docs_run(file, start, count)
                count = 1
                start = number
            end
            prev = number
        end
    end
    module_docs_run(file, start, count)
end

local function check_cfg_imports(file)
    local number, cfg_line = 0, nil
    for line in (read(file) .. "\n"):gmatch("([^\n]*)\n") do
        number = number + 1
        if line:sub(1, 6) == "#[cfg(" then
            cfg_line = cfg_line or number
        elseif line:sub(1, 2) ~= "#[" then
            if cfg_line and line:sub(1, 4) == "use " then
                lint:fail(
                    "cfg imports",
                    file
                        .. ":"
                        .. cfg_line
                        .. ": a #[cfg] use sits at the top level of the file, move it into the mod or function it serves"
                )
            end
            cfg_line = nil
        end
    end
end

local function crate_of(file)
    local crate = fs.dirname(file)
    while crate ~= "." and not fs.is_file(fs.join(root, crate, "Cargo.toml")) do
        crate = fs.dirname(crate)
    end
    return crate
end

local function check_unsafe()
    lint:step("unsafe")
    local allowed = {}
    for _, crate in ipairs(names("-p", "toml", "-o", "json", ALLOWED_UNSAFE, "Cargo.toml")) do
        allowed[crate] = true
    end
    local regex = [[^\s*#!?\[.*\b(allow|expect)\s*\([^)]*\bunsafe_code\b]]
    for _, hit in ipairs(lint:grep(regex, rs_files)) do
        local file = hit.file:gsub("^%./", "")
        local crate = crate_of(file)
        if not allowed[crate] then
            lint:fail(
                "unsafe",
                file
                    .. ":"
                    .. hit.line
                    .. ": allows unsafe_code, but [workspace.metadata.style] allow-unsafe in Cargo.toml does not list the crate at '"
                    .. crate
                    .. "', and only the owner adds one"
            )
        end
    end
end

if dev_os.run({ "cargo", "+nightly", "fmt", "--version" }, { cwd = root, stdout = "null", stderr = "null" }) == 0 then
    lint:check("cargo fmt", { "cargo", "+nightly", "fmt", "--check" })
else
    lint:missing("nightly rustfmt", "rustup toolchain install nightly --component rustfmt", "cargo fmt")
end

lint:check(
    "cargo clippy",
    { "cargo", "clippy", "--all-targets", "--all-features", "--", "-D", "warnings" }
)

if mentions_debug_assertions() then
    lint:check(
        "cargo clippy release",
        { "cargo", "clippy", "--release", "--all-targets", "--all-features", "--", "-D", "warnings" }
    )
end

if lint:have("ast-grep", "brew install ast-grep", "ast-grep scan") then
    if #rs_files > 0 then
        lint:check(
            "ast-grep scan",
            lintset.with_files({ "ast-grep", "scan", "--config", "sgconfig.yml" }, rs_files)
        )
    end
    lint:check("ast-grep test", { "ast-grep", "test", "--config", "sgconfig.yml" })
end

if lint:have("cargo-sort", "cargo install cargo-sort --locked", "cargo sort") then
    lint:check("cargo sort", { "cargo", "sort", "--check", "--workspace" })
end

if lint:have("cargo-deny", "cargo install cargo-deny --locked", "cargo deny") then
    local argv = { "cargo", "deny" }
    if os.getenv("CARGO_NET_OFFLINE") == "true" then
        argv[#argv + 1] = "--offline"
    end
    for _, word in ipairs({ "check", "-W", "unmaintained" }) do
        argv[#argv + 1] = word
    end
    lint:check("cargo deny", argv)
end

if lint:have("yq", "brew install yq", "manifests") then
    local manifests = lint:files({ "Cargo.toml" })
    table.sort(manifests)
    lint:step("manifests")
    for _, manifest in ipairs(manifests) do
        check_manifest(manifest)
    end
end

if lint:have("rg", "brew install ripgrep", "source patterns") then
    check_derive_order()

    lint:step("module docs")
    for _, file in ipairs(rs_files) do
        check_module_docs(file)
    end

    lint:step("cfg imports")
    for _, file in ipairs(rs_files) do
        check_cfg_imports(file)
    end

    lint:step("comment periods")
    for _, file in ipairs(rs_files) do
        for _, number in ipairs(lintset.periods(read(file))) do
            lint:fail("comment periods", file .. ":" .. number .. ": a // one-liner ends in a period, drop it")
        end
    end

    lint:step("comment runs")
    for _, file in ipairs(rs_files) do
        lint:comment_runs(file, "//")
    end

    pattern(
        "getters",
        [[^\s*pub(\([^)]*\))?\s+(const\s+|async\s+|unsafe\s+)*fn\s+get_]],
        "pub fn get_ names an accessor with a verb, name it after the noun"
    )
    pattern(
        "doc headings",
        [[^\s*//[/!]\s*##+\s]],
        "a doc comment heading below h1, use a single # section"
    )

    lint:step("comment spacing")
    for _, file in ipairs(lint:files({ "*.rs", "*.lua", "*.sh" })) do
        if file:find("%.rs$") then
            lint:comment_spacing(file, "//")
        elseif file:find("%.lua$") then
            lint:comment_spacing(file, "--")
        else
            lint:comment_spacing(file, "#")
        end
    end
end

if lint:have("rg", "brew install ripgrep", "unsafe") and lint:have("yq", "brew install yq", "unsafe") then
    check_unsafe()
end

lint:standalone()

-- --- project-specific checks below ---

local json = require("dev.json")

local SCRIPTS = { "static/pure.js", "static/app.js", "static/passkey.js", "static/pages.js", "static/docs.js" }
local PRIVACY_OCTET = "(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])"
local PRIVACY_IP = "(?<![0-9A-Za-z.§])"
    .. PRIVACY_OCTET
    .. "(?:\\."
    .. PRIVACY_OCTET
    .. "){3}(?![0-9A-Za-z]|\\.[0-9])"
local PRIVACY_PATTERN = PRIVACY_IP .. "|\\.ts\\.net\\b|/(?:home|Users)/[a-z][a-z0-9_-]+/"
local PRIVACY_SAFE =
    ":[0-9]+:(127\\.|0\\.0\\.0\\.0$|10\\.|172\\.(1[6-9]|2[0-9]|3[01])\\.|192\\.168\\.|192\\.0\\.2\\.|198\\.51\\.100\\.|203\\.0\\.113\\.)"
local PRIVACY_ALLOW = ".dev/privacy-allow.txt"
local PRIVACY_CHUNK = 500

local SYNTAX_NAMES = {
    ["static/pure.js"] = "pure js syntax",
    ["static/app.js"] = "js syntax",
    ["static/passkey.js"] = "passkey js syntax",
    ["static/pages.js"] = "pages js syntax",
    ["static/docs.js"] = "docs js syntax",
}
for _, script in ipairs(SCRIPTS) do
    lint:check(SYNTAX_NAMES[script], { "node", "--check", script })
end
if lint:have("ast-grep", "brew install ast-grep", "ast-grep scan") then
    lint:check("js trailing commas", {
        "ast-grep",
        "scan",
        "--config",
        "sgconfig.yml",
        "--filter",
        "js-trailing-comma",
        "static",
        "tests/js",
    })
end

local function script_hits(name, message, argv)
    local code = dev_os.run(argv, { cwd = root })
    if code == 0 then
        lint:fail(name, message)
    elseif code ~= 1 then
        lint:fail(name, "rg failed with exit " .. tostring(code))
    end
end

local function check_inline_scripts()
    local name = "no inline scripts"
    lint:step(name)
    local rules = {
        {
            message = "a template holds an inline <script>; move it into static/ (KF-609)",
            paths = { "templates" },
            pattern = [[<script(?![^>]*\ssrc=)(?![^>]*\stype="application/json")]],
        },
        {
            message = "a template holds an inline event handler or javascript: URL; "
                .. "use data-call (KF-609)",
            paths = { "templates" },
            pattern = [[\s(on[a-z]+|hx-on[a-z:-]*)=|javascript:|hx-vals=.js:]],
        },
        {
            message = "a static script builds an inline handler or <script> string (KF-609)",
            paths = SCRIPTS,
            pattern = [[\son[a-z]+=\\?["']|<script]],
        },
    }
    for _, rule in ipairs(rules) do
        local argv = lintset.with_files({ "rg", "-n", "-i", "-P", rule.pattern }, rule.paths)
        script_hits(name, rule.message, argv)
    end
end

local function check_c_build_tools()
    local name = "no c build tools"
    lint:step(name)
    local targets = {
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
    }
    local tools = { "cc", "cmake", "pkg%-config", "bindgen" }
    for _, target in ipairs(targets) do
        local tree, code = dev_os.capture({
            "cargo",
            "tree",
            "--locked",
            "-e",
            "normal,build",
            "--target",
            target,
            "--prefix",
            "none",
        }, { cwd = root })
        if code ~= 0 then
            lint:fail(name, "cargo tree failed for " .. target)
        else
            local found = false
            for line in tree:gmatch("[^\n]+") do
                for _, tool in ipairs(tools) do
                    found = found or line:find("^" .. tool .. " v") ~= nil
                end
            end
            if found then
                lint:fail(name, "a C build tool is in the dependency tree for " .. target)
            end
        end
    end
end

local function tracked_files()
    local out, code = dev_os.capture({ "git", "ls-files", "-z" }, { cwd = root, stderr = "null" })
    if code ~= 0 then
        out = dev_os.capture(
            { "rg", "--files", "-0", "--hidden", "-g", "!.git", "-g", "!target" },
            { cwd = root }
        )
    end
    local files = {}
    for file in out:gmatch("([^%z]+)") do
        local skip = file:find("^scratchpad/") or file:find("Cargo%.lock$") or file:find("^target/")
            or file:find("/target/")
        if not skip then
            files[#files + 1] = file
        end
    end
    return files
end

local function allowed_patterns()
    local argv = { "rg", "-v" }
    local text = fs.read(fs.join(root, PRIVACY_ALLOW)) or ""
    local any = false
    for line in text:gmatch("[^\n]+") do
        if not line:find("^%s*#") and line:find("%S") then
            argv[#argv + 1] = "-e"
            argv[#argv + 1] = line
            any = true
        end
    end
    return any and argv or nil
end

local function check_private_details()
    local name = "private details"
    lint:step(name)
    local files = tracked_files()
    local hits = ""
    for first = 1, #files, PRIVACY_CHUNK do
        local argv = {
            "rg",
            "-P",
            "-o",
            "-n",
            "--no-heading",
            "--with-filename",
            PRIVACY_PATTERN,
            "--",
        }
        for index = first, math.min(first + PRIVACY_CHUNK - 1, #files) do
            argv[#argv + 1] = files[index]
        end
        hits = hits .. dev_os.capture(argv, { cwd = root, stderr = "null" })
    end
    hits = dev_os.capture_input({ "rg", "-v", PRIVACY_SAFE }, hits, { cwd = root })
    local allow = allowed_patterns()
    if allow then
        hits = dev_os.capture_input(allow, hits, { cwd = root })
    end
    if hits ~= "" then
        lint:fail(name, (hits:gsub("\n+$", "")))
    end
end

local function check_parity_history()
    local name = "parity history entries"
    lint:step(name)
    local wanted = {
        "modal-29-history-tab",
        "modal-30-time-in-column",
        "report-01-time-in-column",
    }
    local manifest = json.decode(fs.read(fs.join(root, ".dev/parity/manifest.json")) or "")
    local ids = {}
    for _, file in ipairs(manifest and manifest.include or {}) do
        local doc = json.decode(fs.read(fs.join(root, ".dev/parity", file)) or "")
        for _, entry in ipairs(doc and doc.entries or {}) do
            ids[entry.id] = true
        end
    end
    for _, want in ipairs(wanted) do
        if not ids[want] then
            lint:fail(name, "no parity entry " .. want)
        end
    end
end

check_inline_scripts()
check_c_build_tools()

local site_scripts = lint:files({
    ".dev/standalone/setup.sh",
    ".dev/standalone/pomb-update",
    ".dev/standalone/__publish_release.sh",
    ".dev/site/decoys.sh",
    ".dev/site/firewall.sh",
    ".dev/site/restage.sh",
    ".dev/site/setup-code.sh",
    ".dev/site/status.sh",
})
if lint:have("shellcheck", "brew install shellcheck", "shellcheck") then
    lint:check("shellcheck", lintset.with_files({ "shellcheck", "-x", "-s", "bash" }, site_scripts))
end
if lint:have("shfmt", "brew install shfmt", "shfmt") then
    lint:check("shfmt", lintset.with_files({ "shfmt", "-d", "-i", "2", "-ci" }, site_scripts))
end

check_private_details()
check_parity_history()

return lint:finish()
