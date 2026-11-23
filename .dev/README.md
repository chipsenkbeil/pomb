# dev in this repo

## Using dev

`dev <command>` runs the command `.dev/commands/<command>.luau` or
`.dev/commands/<command>.sh` at the repo root, or a built-in of the same name.
Run `dev` for the curated help and `dev help <command>` for one command.

`dev help -a` lists every command, hidden ones included, and marks a script
that replaces a built-in with `(override)`. `dev help --topics` lists the
topics and `dev help <topic>` prints one. The project's own topics are the
`.md` files in `.dev/help/`.

`-h` after a command prints its description, usage and flags. `--help` after a
command prints the long form.

Global flags go before the command word, as in `dev --remote test`:

- `-v` or `--verbose`: print each command before it runs
- `-vv`: also trace every spawn with its folder, exit code and time
- `--remote`: run the command's job from `.ci/jobs.lua` on ci
- `--local`: run here, whatever `[remote] default` says
- `--detach`: with `--remote`, start the job and return
- `--priority`: with `--remote`, start the job ahead of normal ci jobs
- `--job-timeout DURATION`: with `--remote`, give the ci run a time limit
- `-C DIR`: run as if dev started in `DIR`
- `--agent NAME`: export `DEV_AGENT=NAME` to the command

A command with a ci job runs remote by default. `DEV_REMOTE=0` or `--local`
runs it here, and `--local` with `--remote` is a usage error.

A command gets a flag only when its spec declares it, and a spec refuses an
unknown flag, a missing required argument or an extra word with one sentence
and its usage line on standard error, exit 2.

## Where the files live

- `.dev/commands/`: one `.luau` file per command, returning a `dev.command`
  spec, and `.sh` files for the site scripts. A folder is a command with
  subcommands, and the first line of its `DESCRIPTION` describes it. A command
  word has no hyphen.
- `.dev/lib/`: Luau modules. The `lib` alias in the root `.luaurc` names the
  folder, so `.dev/lib/pomb/cargo.luau` loads with `require("@lib/pomb/cargo")`.
  `lintset.luau` is dev's lint-set module. dev's own modules load as
  `require("@dev/NAME")`.
- `.dev/helpers/`: scripts a command calls and dev never lists. A `.luau`
  helper runs as `dev -l .dev/helpers/NAME.luau WORDS`, and a `.sh` helper
  reaches a command through `$DEV_ROOT`.
- `.dev/tests/`: the `*_test.luau` suites, which `dev test` runs through
  `dev.testrun`. `dev -l .dev/tests/NAME_test.luau` runs one.
- `.dev/standalone/`: shell that runs where dev is absent.

The Rust lint set in `commands/lint.luau` is synced above its
`-- --- project-specific checks below ---` line, and Pomb's own checks sit
below it.

## Commands in this repo

Pomb has the commands below, listed alphabetically, and the `parity`, `site`
and `tools` commands with their subcommands. The helpers are not listed.
`dev help <command>` prints the authoritative text for one command.

### dev build

Build the release binary with `cargo build --release --locked`. Arguments
after `--` go to cargo.

Example:

```sh
dev --remote build
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
`.dev/standalone` needs the `standalone-ok` label on its card. Land takes the
lock `.dev/land.lock` to allow one land at a time to apply, test and push. It
waits for that lock and for `.dev/release.lock` to clear, up to
`POMB_LAND_WAIT_BOUND` seconds each (default 21600, polled every
`POMB_LAND_WAIT_INTERVAL`, default 30), and refuses attribution trailers. It
skips the ci runs when the card's latest `dev-check` note passed `dev --remote
lint` and `dev --remote test` on the same patch. Otherwise it runs `dev
--remote lint` and then `dev --remote test`, and a run refused while the ci hub is
draining repeats every `POMB_DRAIN_INTERVAL` seconds (default 60) up to
`POMB_DRAIN_BOUND` (default 3600).

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
dev --remote nixcheck
```

### dev parity

Check Pomb against the KanbanFlow Golden Master. The scripts run here and
drive Playwright through node, so run them where it is installed (set
`PW_NODE_MODULES` to a directory holding playwright 1.63).

- `dev parity startserver BINARY PORT WORKDIR`: start a Pomb binary with
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
- `dev parity seedepic BASE_URL COOKIE_JAR`: seed the "Pomb Epic Test" board
- `dev parity seedpanel BASE_URL COOKIE_JAR`: seed the "Pomb Panel Test"
  board
- `dev parity seedvisualtest BASE_URL COOKIE_JAR`: seed the "Pomb Visual
  Test" board

Each seed subcommand prints the new board id as the last line of standard
output.

Example:

```sh
dev parity startserver target/release/pomb 8099 /tmp/parity-a
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
`.dev/release.lock` (which `dev land` waits on) and runs `dev --remote lint`,
`dev --remote test`, `dev --remote nixcheck` and `dev --remote build` at the
same time on ci, and pushes main only when all four pass and main has not
moved. It then tags `batch-N` and `vVERSION` and prints the nix hash and the
request block. A `--dry-run` leaves the release worktree behind, and
`dev verify --cleanup` removes it.

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
- `dev site setupcode`: print the setup code each channel logged at its
  first start
- `dev site firewall`: drop new connections to port 22 except from the box
  itself, so only the tunnel reaches sshd (idempotent)
- `dev site decoys` (hidden, listed by `dev help -a`): remove the files a
  verifier planted in the nightly backups directory

Example:

```sh
dev site verify nightly --wait
```

### dev test

Run the test suite. Arguments after `--` go to nextest, and doctests run only
when there are none. Each `tests/js` script then runs with node against
`static/app.js`, and every `.dev/tests/*_test.luau` runs last.

Example:

```sh
dev --remote test
dev test -- -j 1 module
```

### dev tools seedgoldenmaster

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
dev tools seedgoldenmaster --as-of 2026-09-30 http://127.0.0.1:8099 jar.txt
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
