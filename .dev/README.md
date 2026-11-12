# dev in this repo

## Using dev

`dev <verb>` runs the verb `.dev/<verb>.lua` or `.dev/<verb>.sh` at the repo
root, or a built-in of the same name. Run `dev` for the curated help and
`dev help <verb>` for one verb.

`dev help -a` lists every command, hidden ones included, and marks a script
that replaces a built-in with `(override)`. `dev help --topics` lists the
topics and `dev help <topic>` prints one. The project's own topics are the
`.md` files in `.dev/topics/`.

`-h` after a verb prints its description, usage and flags. `--help` after a
verb prints the long form.

Seven global flags work before a `--` on every verb:

- `-v` or `--verbose`: print each command before it runs
- `-vv`: also trace every line of dev and of each bash script it runs
- `--remote`: run the verb's job from `.ci/jobs.lua` on ci
- `--local`: run here, whatever `[remote] default` says
- `--detach`: with `--remote`, start the job and return
- `-C DIR`: run as if dev started in `DIR`
- `--agent NAME`: export `DEV_AGENT=NAME` to the command

A command with a ci job runs remote by default. `DEV_REMOTE=0` or `--local`
runs it here, and `--local` with `--remote` is a usage error.

A script gets a flag only when its `flags:` header line declares it. Everything
after the first `--` reaches the script untouched.

## The dev modules

A Lua verb loads each module below with `require` by its dotted name. The
doc comments of the module hold every function.

### dev.verb

The helpers a verb needs: `usage_error(ctx, msg)` exits 2, `no_args(ctx)`
refuses any positional argument, `note(msg)` writes to standard error,
`die(msg)` exits 1, `show(ctx, argv)` prints `+ <argv>` at `-v`, and
`need(binary, install)` exits 1 when `binary` is not on `PATH`.

```lua
local verb = require("dev.verb")

local ctx = ...
verb.no_args(ctx)
verb.need("rg", "brew install ripgrep")
verb.note("searching " .. ctx.root)
```

### dev.fs

File and path helpers: `exists`, `is_dir`, `is_file`, `is_link`, `is_exec`,
`read`, `write`, `mkdir`, `remove`, `list`, `stat`, `realpath`, `readlink`,
`mktemp`, `symlink`, `join`, `dirname` and `basename`. A call that can fail
returns nil and a message.

```lua
local fs = require("dev.fs")

local path = fs.join(ctx.root, ".dev", "config.toml")
if fs.is_file(path) then
    local text, err = fs.read(path)
    print(text or err)
end
```

### dev.os

Run and capture commands with `run`, `capture`, `capture_input` and
`capture_timed`, and find a program with `which`. `exec`, `getuid`, `isatty`
and `quote` live here too.

```lua
local dev_os = require("dev.os")

local out, code = dev_os.capture({ "git", "-C", ctx.root, "status", "--short" })
if code == 0 and out ~= "" then
    print(out)
end
```

### dev.ui

Terminal drawing: `context`, colors through `paint` and `mark`, `table`, `bar`,
`confirm` and `spin`. Color is on only when the stream is a terminal,
`NO_COLOR` is empty or unset and `TERM` is not `dumb`.

```lua
local ui = require("dev.ui")

local view = ui.context()
local rows = { { "lint", "ok" }, { "test", "ok" } }
for _, line in ipairs(ui.table(view, { "job", "state" }, rows)) do
    print(line)
end
```

### dev.text

Display-width text helpers: `width`, `fit`, `words`, `wrap`, `trim` and
`contains`. `wrap` fills lines from a word list and leaves a word wider than
the width on a line of its own.

```lua
local text = require("dev.text")

local lines = text.wrap(text.words("one two three four"), 10)
for _, line in ipairs(lines) do
    print(text.fit(line, 8))
end
```

### dev.config

Reads `~/.config/dev/config.toml` and then `<root>/.dev/config.toml` over it.
`get(root, "a.b")` reads a dotted key, `prefer_user(root, "a.b")` lets the
user file win, and `user("a.b")` reads the user file alone.

```lua
local config = require("dev.config")

local mode = config.get(ctx.root, "remote.default")
print(mode or "unset")
```

### dev.project

`root()` finds the project root, `detect_type(root)` names the project's
language from the files in its root, `dotfiles_root()` finds the dotfiles
clone that holds dev, and `share` is dev's own share directory.

```lua
local project = require("dev.project")

local root = project.root()
print(root, project.detect_type(root) or "no type")
```

### dev.vcs

Picks the project's backend and lists what it holds. `backend(root)` returns
the jj or git backend, and `remotes(root, backend)` and
`branches(root, backend)` list names.

```lua
local vcs = require("dev.vcs")
local verb = require("dev.verb")

local backend, err = vcs.backend(ctx.root)
if not backend then
    return verb.die(err)
end
for _, name in ipairs(vcs.branches(ctx.root, backend)) do
    print(name)
end
```

### dev.templates

The lint set directory and its manifest. `set_for(kind)` names the set for a
project type, `dir(kind)` returns its directory, and `manifest(dir, kind)`
lists the files the set copies into a project.

```lua
local project = require("dev.project")
local templates = require("dev.templates")

local kind = project.detect_type(ctx.root)
local dir = kind and templates.dir(kind)
if dir then
    for _, entry in ipairs(templates.manifest(dir, kind)) do
        print(entry.dest)
    end
end
```

## Adding a verb, a subcommand or a flag in Lua

### A verb

`.dev/VERB.lua` is a verb. A `.lua` file needs no exec bit and wins over a
`.sh` file of the same name. dev loads the file and calls the chunk with one
argument, the context, which `local ctx = ...` reads.

- `ctx.name`: the command as typed
- `ctx.words`: the command words
- `ctx.root`: the project root, an absolute path
- `ctx.args`: the positional arguments, with each `KEY=value` word whose key
  starts with an uppercase letter removed, unless the header has a
  `positional` line
- `ctx.vars`: the `KEY=value` arguments whose key starts with an uppercase
  letter, by key, and empty under a `positional` line
- `ctx.after`: how many of `ctx.args` follow a `--`
- `ctx.dashed`: whether a `--` was typed
- `ctx.verbose`: 0, 1 or 2
- `ctx.interpreter`: the luajit dev runs under

The chunk returns the exit code as an integer, and nothing means 0. A Lua
error ends the run with `dev <name>: <message>` and exit 1.

The first header line that is not a `group:`, `hidden`, `local`, `positional`
or `flags:` line is the description in `dev help`. This verb runs as
`dev greet NAME=you`.

```lua
--[[
Print a greeting.
usage: greet [NAME=who]
]]
local verb = require("dev.verb")

local ctx = ...
verb.no_args(ctx)
print("hello " .. (ctx.vars.NAME or "world"))
return 0
```

### A subcommand

`.dev/NOUN/SUB.lua` runs as `dev NOUN SUB`. A directory of scripts is a noun,
and the first line of `.dev/NOUN/DESCRIPTION` describes it.

A noun with no subcommand, or an unknown one, exits 2 and lists the
subcommands. Saved as `.dev/cache/show.lua`, this verb runs as
`dev cache show` and prints `cache show`.

```lua
--[[
Print the noun's state.
]]
local verb = require("dev.verb")

local ctx = ...
verb.no_args(ctx)
print(ctx.name)
```

### A flag

A script gets a flag only when its header declares it. Before a `--`, any
other word that starts with `-` stops dev with exit 2, and `-h` and `--help`
stay reserved, so a `flags:` line cannot declare them.

```lua
--[[
Print a greeting.
usage: greet [--loud]
flags: --loud
]]
local verb = require("dev.verb")

local ctx = ...
local loud = false
for _, arg in ipairs(ctx.args) do
    if arg == "--loud" then
        loud = true
    else
        verb.usage_error(ctx, "unknown argument " .. arg)
    end
end
print(loud and "HELLO" or "hello")
```

### Group and hidden lines

`group: <name>` sets the heading `dev help` lists the verb under, and a verb
with none lands in a group named after the project directory. `hidden` keeps
the verb out of `dev help` and `dev help -a` still lists it. Scripts named
`__*.sh` or `__*.lua` never appear in any listing.

```lua
--[[
group: Check your work
hidden
Print the checks this repo runs.
]]
local verb = require("dev.verb")

local ctx = ...
verb.no_args(ctx)
print("lint")
```

### Your own modules

`.dev/lua/` is on `package.path` while a Lua verb runs, so
`.dev/lua/myproj/checks.lua` loads with `require("myproj.checks")`. dev never
lists `lua` or `topics` as a noun.

## Commands in this repo

Pomb has the verbs below, listed alphabetically, and the `parity`, `site` and
`tools` nouns with their subcommands. Hidden `__` helpers are not listed.
`dev help <verb>` prints the authoritative text for one verb.

### dev build

Build the release binary with `cargo build --release --locked`. Arguments
after `--` go to cargo.

Example:

```sh
dev build --remote
```

### dev fmt

Format the workspace with `cargo +nightly fmt`, which the unstable options in
`rustfmt.toml` need, then drop the trailing period from each `//` one-liner.
It takes no flags of its own.

Example:

```sh
dev fmt
```

### dev land

Apply a verified commit onto the remote's main and push it once lint and test
pass on ci.

- `<verified-commit>`: positional, the commit to land
- `--card ID`: required, the card the commit belongs to

The commit must be exactly one commit past its merge-base with the remote's
main and must not change the package version. A commit that adds a file under
`.dev/standalone` needs the `standalone-ok` label on its card. Land waits for
`.dev/release.lock` to clear, up to `POMB_LAND_WAIT_BOUND` seconds (default
5400, polled every `POMB_LAND_WAIT_INTERVAL`, default 30), and refuses
attribution trailers. It skips the ci runs when the card's latest `dev-check`
note passed `dev --remote lint` and `dev --remote test` on the same patch.
Otherwise it runs `dev lint --remote` and then `dev test --remote`, and a run
refused while the ci hub is draining repeats every `POMB_DRAIN_INTERVAL`
seconds (default 60) up to `POMB_DRAIN_BOUND` (default 3600).

A push refused after main moved is applied again on the new main, 3
attempts in all, and never forced. On success the card moves to Release and
its worktree and branch are removed.

Example:

```sh
dev land 3f9c2ab --card 59d276ed
```

### dev lint

Run the Rust lint set. Cargo fmt, clippy, ast-grep, cargo-sort and cargo-deny
run first, then the manifest and source checks and the project checks. Every
failed check is named at the end, and the exit code is non-zero when any
failed. It takes no flags of its own.

Example:

```sh
dev --remote lint
```

### dev lock

Resolve `Cargo.lock` for one dependency on ci, with no cargo run on this
machine.

- `PKG`: positional, the package to update
- `--precise VER`: pin the package to `VER`

A ci job runs `cargo update -p PKG` on a copy of the working copy and builds
and tests nothing. The job's `Cargo.lock` is fetched with `ci cp` and written
over this tree's `Cargo.lock`, and nothing else changes.

Example:

```sh
dev lock serde --precise 1.0.200
```

### dev nixcheck

Run the test archive in user, mount and network namespaces with only the
tools the Nix derivation provides on `PATH`, so a missing tool fails before
release. Linux only. Arguments after `--` go to nextest.

Example:

```sh
dev nixcheck --remote
```

### dev parity

Check Pomb against the KanbanFlow Golden Master. The scripts run here and
drive Playwright through node, so run them where it is installed (set
`PW_NODE_MODULES` to a directory holding playwright 1.63).

- `dev parity start-server BINARY PORT WORKDIR`: start a Pomb binary with
  `--auth none` under a frozen server clock, seed the three parity boards and
  write `WORKDIR/state.json` (needs libfaketime, and `PARITY_SEED_TIME`,
  `PARITY_SERVE_TIME`, `PARITY_TZ` and `FAKETIME_LIB` tune it)
- `dev parity run --state FILE [--manifest FILE] [--out FILE] [--only ID,ID]
  [--debug-dir DIR]`: replay the parity manifest and compare it with the
  goldens
- `dev parity regress --candidate FILE --baseline FILE --out DIR [--manifest
  FILE] [--only ID,ID] [--max-diff N] [--fuzz PCT%]`: diff the screenshots of
  two running builds entry by entry (`--max-diff`: differing pixels allowed,
  default 0, and `--fuzz`: per-channel tolerance, default 1%)
- `dev parity suggest --state FILE --entry ID [--manifest FILE]`: print the
  Pomb element that overlaps each golden element of a manifest entry
- `dev parity finder --candidate FILE`: check the task finder of a candidate
  server against its state file
- `dev parity overflow STATE_FILE WIDTH`: check that the candidate pages do
  not scroll sideways at a phone width
- `dev parity seed-epic BASE_URL COOKIE_JAR`: seed the "Pomb Epic Test" board
- `dev parity seed-panel BASE_URL COOKIE_JAR`: seed the "Pomb Panel Test"
  board
- `dev parity seed-visual-test BASE_URL COOKIE_JAR`: seed the "Pomb Visual
  Test" board

Each seed subcommand prints the new board id as the last line of standard
output.

Example:

```sh
dev parity start-server target/release/pomb 8099 /tmp/parity-a
dev parity run --state /tmp/parity-a/state.json --out report.json
```

### dev regress

Diff this tree's screenshots against a baseline revision on a ci browser
runner.

- `[REV]`: positional, the baseline revision (default: the merge-base of
  `HEAD` and main on the remote of record)

The baseline sha prints to stderr before the run. The working tree is never
written. The report, diff images included, is kept as a ci artifact, and the
run id and the `ci cp` command that fetches it print at the end.

Example:

```sh
dev regress
```

### dev release

Cut a release from the remote's main as it stands and print the homelab
request.

- `--batch N`: the batch number, required unless `--hash-only` is given
- `--summary TEXT`: the release commit summary, required with `--batch`
- `--env KEY=VALUE`: list a `pomb.service` environment change in the request
  (repeatable)
- `--dry-run`: do everything except the push, the tags and the hash
- `--hash-only SHA`: print the request block for an already pushed full
  40-character sha

Release refuses unless main has a commit since its last `v*` tag. It bumps
the version in `.dev/worktrees/release-N` and commits `Release batch-N:
SUMMARY` with only `Cargo.toml` and `Cargo.lock`. It takes the lock
`.dev/release.lock`, runs `dev lint --remote`, `dev test --remote`,
`dev nixcheck --remote` and `dev build --remote` at the same time on ci, and
pushes main only when all four pass and main has not moved. It then tags
`batch-N` and `vVERSION` and prints the nix hash and the request block. A
`--dry-run` leaves the release worktree behind, and `dev verify --cleanup`
removes it.

Example:

```sh
dev release --batch 12 --summary "Add card filters" --env POMB_FOO=1
dev release --hash-only 3f9c2ab4d5e6f708192a3b4c5d6e7f8091a2b3c4
```

### dev site

Host the public Pomb sites. Each subcommand runs here and takes a channel,
`stable` or `nightly`, where it needs one.

- `dev site deploy <stable|nightly> [<tag>]`: run the box's pull agent now
- `dev site verify [<stable|nightly> [<expected>]] [--wait]`: check a channel
  after an update (`<expected>`: the release tag or commit sha to expect,
  `--wait`: poll for up to 1200 seconds)
- `dev site status`: show service states, current releases, last update
  results and disk use
- `dev site provision`: provision and harden the box over SSH (idempotent)
- `dev site launch <stable|nightly>`: enable and start a channel's service
  and update timer
- `dev site restage nightly [--bad]`: make the next update real (`--bad`:
  stage a binary that exits 1)
- `dev site setup-code`: print the setup code each channel logged at its
  first start
- `dev site decoys` (hidden, listed by `dev help -a`): remove the files a
  verifier planted in the nightly backups directory

Example:

```sh
dev site verify nightly --wait
```

### dev test

Run the test suite. Arguments after `--` go to nextest, and doctests run only
when there are none. Each `tests/js` script then runs with node against
`static/app.js`, and every `.dev/tests/*_test.lua` runs last.

Example:

```sh
dev --remote test
dev test -- -j 1 module
```

### dev tools seed-golden-master

Reproduce the KanbanFlow Golden Master board on a Pomb instance.

- `--as-of YYYY-MM-DD`: shift every fixture date by the whole days between
  2026-09-30 and this day (default: today)
- `BASE_URL`: positional, the server, never production
- `COOKIE_JAR`: positional, a curl cookie jar (may be empty against a server
  without sign-in)
- `[FIXTURE_JSON]`: positional (default: `.dev/golden-master-board.json`)

It creates a new board, leaves existing boards alone and prints the board id
as the last line of standard output.

Example:

```sh
dev tools seed-golden-master --as-of 2026-09-30 http://127.0.0.1:8099 jar.txt
```

### dev verify

Check a release on green-box and clean up its worktrees.

- `SHA`: positional, the release sha
- `--backup FILE`: a file name or a path under `/var/lib/pomb-backups/`, also
  checks that the named database copy exists on the host
- `--cleanup WORKTREE...`: remove the named `.dev/worktrees/*` and delete
  their branches when fully merged into main
- `--delete-branch BRANCH`: also delete the named branch when unmerged
  (repeatable)

The read-only checks run over ssh and print PASS or FAIL each: `build_sha`
equals the release sha, prod `app.js` equals the release tree's, a
cross-site POST gets 403, the journal shows the schema and listening lines,
public `/login` answers, `servers[0].url` in OpenAPI is the public URL, and a
bad Bearer token gets `token_invalid`. Any FAIL exits non-zero and nothing
else happens.

Example:

```sh
dev verify 3f9c2ab --backup pomb-2026-11-11.redb
dev verify 3f9c2ab --cleanup .dev/worktrees/release-12
```
