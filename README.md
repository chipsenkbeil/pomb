# Pomb

A self-hosted kanban board with pomodoro-sized cards, WIP limits, swimlanes
and a pomodoro timer (ding and browser notification at the end, overtime,
per-task pomodori and interruptions). Agents work the same boards over a
JSON API.

Pure Rust, one static binary, one database file. No Docker needed.

## Quickstart

```sh
cargo install --git https://github.com/chipsenkbeil/pomb
pomb
```

Open http://localhost:3000. By default Pomb has no sign-in and listens on
`127.0.0.1` only, so nobody else can reach it.

Prebuilt static x86_64 Linux binaries are on GitHub Releases: `nightly` is
the newest `main` build and `latest` the newest `v*` tag.

```sh
base=https://github.com/chipsenkbeil/pomb/releases/download/nightly
curl -LO "$base/pomb-x86_64-unknown-linux-musl"
curl -LO "$base/pomb-x86_64-unknown-linux-musl.sha256"
sha256sum -c pomb-x86_64-unknown-linux-musl.sha256
```

## Pointing an agent at it

Everything an agent needs is served by your Pomb instance:

- `/skill.md`: the agent skill (workflow, pairing, claiming, events).
- `/llms.txt`: an index of the agent documents.
- `/docs/api.md`: the API reference.
- `/api/v1/openapi.json`: the OpenAPI contract.

Install the skill for Claude Code (the **Settings** page shows this line
with your server's URL):

```sh
mkdir -p ~/.claude/skills/pomb && curl -fsS https://pomb.example.com/skill.md -o ~/.claude/skills/pomb/SKILL.md
```

Give the agent two environment variables:

- `POMB_URL`: the base URL, such as `https://pomb.example.com`.
- `POMB_API_TOKEN`: a `pomb_...` token from **Settings > API tokens**.

Without a token the agent pairs through the device flow: it shows you a
code, you approve it at `/device`, and it receives a token once. Any agent
without the skill can start from one prompt:

```text
Read https://pomb.example.com/skill.md and follow it to work my "<board>" board.
```

A token has one scope: `read`, `work` (claim, move, comment, log time),
`provision` (`work` plus creating boards and adding columns, swimlanes and
fields) or `admin` (everything, deletes included). Give agents `work` or
`provision`. Pomb has no recycle bin. Agents wait for board changes with
`GET /api/boards/{id}/events/wait` instead of polling.

## Configuration

Flags or the matching environment variables. Bare `pomb` works.

- `--auth` / `POMB_AUTH`: `none` (default) or `passkey`, see Signing in.
- `--mode` / `POMB_MODE`: `single` (default, one account) or `multi` (the
  admin plus invited or approved members).
- `--host` / `HOST`: listen address (default `127.0.0.1`).
- `--port` / `PORT`: HTTP port (default `3000`).
- `--database-path` / `DATABASE_PATH`: database file (default
  `~/.local/share/pomb/pomb.redb`, honoring `XDG_DATA_HOME`).
- `--backup-dir` / `POMB_BACKUP_DIR`: backup directory (default `backups`
  beside the database).
- `--public-url` / `POMB_PUBLIC_URL`: external base URL, such as
  `https://pomb.example.com`. Browser writes must come from its origin, and
  it names the device-pairing URL and the OpenAPI server. Required by
  `--auth passkey`.
- `--trust-proxy` / `POMB_TRUST_PROXY`: `none` (default), `cloudflare`
  (`CF-Connecting-IP`) or `forwarded` (last `X-Forwarded-For`), read only on
  a loopback connection, to name the client for per-address limits.

## Signing in

Pomb has no passwords. API tokens work the same in both modes.

- `--auth none`: every browser request is yours, so Pomb starts only on a
  loopback `--host` and answers only loopback host names and the
  `POMB_PUBLIC_URL` host. With a non-loopback `POMB_PUBLIC_URL` and no
  explicit `--auth`, it refuses to start.
- `--auth passkey`: sign in with a passkey bound to the `POMB_PUBLIC_URL`
  host. While no passkey exists, each start logs a one-time setup code
  (`pomb: setup code ABCD-EFGH ...`) that the setup page asks for.
  **Settings > Passkeys** adds and removes passkeys. Sessions last 30 days.

Lost every passkey in multi mode: open `/recover` with your handle and
recovery code. In single mode, restart with `--auth none` on a loopback
host (keeping `POMB_PUBLIC_URL`), add a passkey at the public URL, and
restart with `--auth passkey`. Anyone who reaches the URL in between acts as
you.

## Accounts

`--mode multi` needs `--auth passkey`. Each member sees only their own
boards, tasks, templates, time, tokens and settings. The admin page,
`/admin`, approves, suspends and deletes members, creates single-use invite
links (1 to 30 days) and sets the registration mode. With requests open,
anyone can sign up at `/signup` and waits for approval. A request waits at
most 30 days, and at most 100 by default wait at once.

**Admin > Limits** sets per-account quotas, request rates and the storage
limits. A write past a quota answers 409 `quota_exceeded`, and a write while
the database is over its size limit or the disk under its free-space floor
answers 507 `storage_full`. A member board unused for 180 days is deleted
after a 14-day warning (**Admin > Unused boards**). Single mode never
deletes anything.

Every account gets a recovery code at sign-up. Suspending an account signs
it out and deletes its tokens. `POMB_BOOTSTRAP_TOKEN` is a standing admin
credential, so multi mode refuses to start while it is set.

## Backups

Backups are off by default. **Settings > Backups** turns them on and sets
the interval (1 to 168 hours, default 24) and how many to keep (1 to 90,
default 7). Each is a consistent snapshot named `pomb-<UTC time>.redb` in
the backup directory, and **Back up now** takes one immediately. Backups
copy the whole file, so whoever runs the server can read every account.

To restore:

1. Stop Pomb.
2. Copy the backup over the database file.
3. Start Pomb.

A backup from a newer Pomb than the one you start is refused.

## Board archives

**Board menu > Archive** downloads one board as `<board name>.tar.gz`: a
`board.json` plus each attachment at `attachments/<attachment id>/<file
name>`. The JSON holds the board and its settings, columns, swimlanes,
colors, custom fields and every task with its comments, history, links and
time entries. It names people by handle, and every id in it points at a row
inside the file. It holds no token, passkey, session or calendar-feed link.
`GET /api/boards/{id}/archive` serves the same file to API clients.

**Boards > Import board** creates a new board from such a file, and
`POST /api/boards/import` takes the `.tar.gz` as the request body. Every id is
fresh: importing never overwrites or merges into a board you have, and
importing the same file twice makes two independent boards. A task keeps your
account as its assignee when the archive names your handle, and any other handle
stays as text in the task's metadata. A link to a task outside the archive and an
attachment the archive lists with a null `file` are dropped, and the response counts them.
An attachment whose `file` names a path the archive does not hold refuses the whole archive. The file
is read whole before anything is stored, and an archive that breaks a quota, a
text cap or a limit of the import (64 MiB packed, 256 MiB unpacked, 32 MiB of
`board.json`, 50,000 files) is refused with nothing written.

To archive every board of every account without a server:

```text
pomb archive --database-path pomb.redb --out archives
```

This writes `archives/<account handle>/<board name>-<board id>.tar.gz`. It
opens the file read-only, so it refuses a file that a running Pomb holds, one
that was not closed cleanly, and one from a newer or older schema than this
binary reads. Start Pomb on an older file once to upgrade it, then stop Pomb
and archive.

## Using it

- Drag cards between columns and swimlanes. Dropping into a Done column
  stamps the completion date, and dragging out clears it.
- Columns and swimlanes are customizable on the board: add, rename,
  reorder, delete and set WIP limits.
- A card opens a modal with its Markdown description (checklists tick in
  place), subtasks, comments, fields, links and time. The rail starts a
  pomodoro or stopwatch.
- The header timer pill follows the active session across pages. Stopping
  early asks why, and sessions under 20 seconds are discarded.
- Card colour encodes size: 1 pomodoro yellow, 2 green, 3 blue, more red.

## Building

The toolchain is pinned in `rust-toolchain.toml`. `cargo run` serves a debug
build with `static/` read from disk. Release builds embed the web assets.

Every check runs through `dev` (scripts in `.dev/`, details in
`AGENTS.md`):

- `dev fmt`: format.
- `dev lint`: fmt check, clippy, ast-grep and the project checks.
- `dev test`: nextest and doc tests.
- `dev build`: release build.
- `dev regress`: screenshot regression against a base commit.

Add `--remote` to run lint, test or build on the homelab ci instead of the
local machine.

htmx and SortableJS are vendored in `static/vendor/` as the published
minified files:

| File | Version | SHA-256 |
|---|---|---|
| `static/vendor/htmx-1.9.12.min.js` | htmx 1.9.12 | `449317ade7881e949510db614991e195c3a099c4c791c24dacec55f9f4a2a452` |
| `static/vendor/sortable-1.15.2.min.js` | SortableJS 1.15.2 | `ca68430703c4f5960e90735867c6e94d29b5a3de37107d8100e5a301007e9e6e` |

## Releasing and deploying

1. `dev release <commit> --batch N --summary "..."` squashes a verified
   commit onto `main`, runs lint, test and build on ci, pushes, and prints
   the sha and nix hash for deployments that pin a commit.
2. A push to `main` publishes the `nightly` binary, and a `v*` tag publishes
   a release (`.github/workflows/publish.yml`).
3. After a deployment switches to the new sha, `dev verify <sha> --batch N`
   checks it is live and marks the tracker.

`nightly.pomb.app` follows `nightly` and `pomb.app` follows `v*` tags. Their
hosting lives in a separate repository.
