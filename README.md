# Pomb

A self-hosted kanban board inspired by KanbanFlow, with pomodoro-sized task
cards, WIP limits, swimlanes, and a full pomodoro timer: ding + browser
notification when a session ends, overtime counting, per-task pomodori and
interruption tracking, and a "why did you stop?" menu when you bail early.

Pure Rust, one static binary, one database file. No Docker needed.

## Quickstart

```sh
cargo install --git https://github.com/chipsenkbeil/pomb
pomb
```

Open http://localhost:3000. By default Pomb has no sign-in and listens on
127.0.0.1 only, so the board is yours on this machine and nobody else can
reach it.

Configuration: flags or the matching environment variables. Everything has
a sane default, so bare `pomb` just works.

- `--auth` / `POMB_AUTH`: how a browser signs in (default: `none`, see below).
- `--mode` / `POMB_MODE`: `single` (default) serves one account, `multi`
  serves the admin plus the people it invites or approves (see Accounts
  below).
- `--host` / `HOST`: address to listen on (default: `127.0.0.1`).
- `--port` / `PORT`: HTTP port (default: `3000`).
- `--database-path` / `DATABASE_PATH`: database file
  (default: `~/.local/share/pomb/pomb.redb`, honoring `XDG_DATA_HOME`).
- `--backup-dir` / `POMB_BACKUP_DIR`: where backups are written
  (default: a `backups` directory beside the database file).
- `--public-url` / `POMB_PUBLIC_URL`: externally visible base URL, such as
  `https://pomb.example.com` (scheme, host and optional port, no path).
  It sets the device-pairing `verification_url` and the OpenAPI `servers`
  entry, and browser writes must come from its origin. Unset, the request
  `Host` header is used. `--auth passkey` needs it.
- `--trust-proxy` / `POMB_TRUST_PROXY`: which proxy on this host names the
  client for the per-address limits (default: `none`, the connection's own
  address). `cloudflare` reads `CF-Connecting-IP` from cloudflared, and
  `forwarded` reads the last `X-Forwarded-For` entry from a reverse proxy
  such as Caddy. Either header counts only on a loopback connection, and a
  proxied request without it counts as the proxy, which is logged once.

## Signing in

Pomb has no passwords. It runs in one of two modes, and API tokens work the
same in both.

- `--auth none` (the default): no sign-in. Every browser request is yours, so
  Pomb refuses to start unless `--host` is a loopback address (`127.0.0.0/8`,
  `::1` or `localhost`). It also answers only to loopback host names and the
  `POMB_PUBLIC_URL` host, so another site cannot reach it through DNS
  rebinding. A reverse proxy in front would still open the board to everyone
  it serves, so when neither `--auth` nor `POMB_AUTH` is given and
  `POMB_PUBLIC_URL` has a host that is not loopback, Pomb refuses to start.
  Choose `--auth passkey`, or name `--auth none` if the network in front of
  the proxy is trusted. Combining `--auth none` with any `--trust-proxy` other
  than `none` also refuses to start.
- `--auth passkey`: you sign in with a passkey. `POMB_PUBLIC_URL` is required,
  its host (a DNS name, not an IP address) is the passkey relying party, and
  Pomb may listen on any address, for example behind a reverse proxy. While
  no passkey exists, each start prints a one-time setup code to the log
  (`pomb: setup code ABCD-EFGH ...`). The setup page asks for that code and a
  username and registers your first passkey, and the code then stops
  working. **Settings > Passkeys** lists your passkeys and adds or removes
  them, though the last one always stays. Removing a passkey signs every
  other browser out, and **Sign out other sessions** there does the same. A
  browser session ends on the server after 30 days.

A database from a release with passwords opens fine. The password is
dropped, and under `--auth passkey` the setup page registers the existing
user's first passkey with the setup code from the log. The upgrade also
ends every browser session, since each was opened with a password.

### Lost every passkey

Under `--mode multi`, open `/recover` and enter your handle and recovery code
(see Accounts). In single mode there is no recovery code. Instead:

1. Restart with `--auth none` and `--trust-proxy none` on a loopback
   `--host`, keeping `POMB_PUBLIC_URL` set.
2. Open **Settings > Passkeys** at the public URL and add a passkey. Passkeys
   are bound to the public URL's host, so the page must be opened there and
   not at `localhost`. Until step 3, anyone who reaches that URL acts as you,
   so keep this short.
3. Restart with `--auth passkey`.

## Accounts

By default Pomb runs in single mode: one account, the admin, owns every
board. `--mode multi` (`POMB_MODE=multi`) also serves members, each with only
their own boards, tasks, templates, time, tokens and settings. Multi mode
needs `--auth passkey`, and Pomb refuses to start without it.

- Switching a single instance to multi keeps every board with the admin and
  is logged at startup. Switching back is refused while more than one
  account exists.
- The admin page, `/admin` (linked from **Settings**), lists every account
  with its handle, role, status, creation, last-seen time and board count,
  never its boards. There the admin approves, suspends, unsuspends and deletes
  member accounts, picks the registration mode and manages invite links.
- The admin creates single-use invite links under **Admin > Invites**, each
  valid for 1 to 30 days (7 by default), and can revoke unused ones. Opening a
  link shows the sign-up page, where the invited person picks a handle (3 to
  32 of `a-z`, `0-9`, `_` and `-`) and registers a passkey.
- Registration is by invite only unless the admin sets **Admin >
  Registration** to take requests. Then anyone can sign up at `/signup`, and
  the new account waits for approval: it signs in to a page that says so and
  reaches nothing else, and it gets its first board once approved. A request
  left waiting for 30 days is deleted, and once as many requests as the
  admin's cap (100 by default) wait, new ones are refused.
  Invite links keep creating active accounts.
- **Admin > Limits** sets per-account quotas (by default 20 boards per
  account, 2000 tasks, 50 columns, 20 swimlanes and 50 custom fields per
  board, 500 comments, 20 attachments and 200 subtasks per task, 50 saved
  templates, 20000 time entries, and 32 MiB of boards, tasks, time entries,
  templates and attachment files per account, none for the admin, and none
  for the starter board),
  the request rates (by default 100 requests at once
  and 20 a second per API token, 500 at once and 100 a second per account
  across its sessions and tokens, and 10 sign-in requests a minute per
  address) and the storage limits. A write past a quota answers 409
  `quota_exceeded`, except a stopped timer, whose session is still recorded
  up to 1 MiB past the stored-bytes quota. A stop the time entry quota
  refuses answers 409 too and leaves the timer running. Each account, the
  admin included, holds at most 50 API tokens and 10 passkeys and starts at
  most 500 time entries a UTC day. A task row holds at most 1 MiB, a custom
  field definition 256 KiB and a saved template 8 MiB. The account list shows
  what each account stores.
- Text has fixed caps: 500 characters for a task or subtask name, 64 KiB
  for a description, 5000 bytes for a comment, 50 labels of 50 characters,
  200 characters for column, swimlane, field, option and template names,
  500 for a color description, 4 KiB for a text field value, a table cell or
  a time entry's note, and 1 KiB for the reason a timer stopped. Time entry
  labels follow the task label caps. Longer input answers 400
  `invalid_request` naming the field and its cap. A request body may hold
  256 KiB, and an attachment 5 MiB.
- When the database or its file holds more than its size limit (512 MiB by
  default, multi mode only) or the disk has less free space than the floor
  (256 MiB by default, in both modes), writes that add data answer 507
  `storage_full` and every page shows a banner. They also stop when the
  disk cannot be measured. A backup that would leave
  less than the floor free is refused. Deleting, signing in and out,
  recovery, account deletion and the admin's actions keep working. In single
  mode **Settings > Storage** sets the floor.
- Deletes shrink the data the database holds but not its file until Pomb
  restarts (it compacts the file at each start). To recover from a full
  database, delete data and restart the service. **Admin > Limits** shows the
  file size and the data held against the limit, with a warning from 80%.
- A member board that nobody uses for 180 days is deleted with everything on
  it, attachments included, and a member left without a board is deleted
  too. Any signed-in request or API token call that reaches the board or one
  of its tasks counts as a use, and so does a calendar app reading the
  board's feed. For its last 14 days the board shows a warning and the board
  list marks it, and opening it keeps it. **Admin > Unused boards** turns
  this off or changes both numbers (30 to 3650 days, and a warning shorter
  than that), and shows the last daily run and what it deleted. Turning
  pruning on or shortening its limit deletes nothing until a full warning
  period has passed. The admin's boards and those of pending and suspended
  accounts are never pruned, lifting a suspension counts as a use of every
  board of the account, and single mode never deletes anything.
- Suspending an account signs it out and deletes its API tokens, and its
  boards stay. Lifting the suspension lets it sign in again, without the old
  tokens.
- Every account gets one recovery code, shown once at sign-up and stored only
  as a hash. With its handle it registers a new passkey at `/recover`, which
  also deletes the account's API tokens and every other passkey, signs out
  every other browser, says how many it revoked and shows a new code.
  **Settings > Recovery code** replaces it. On first setup in multi mode the
  admin gets one too, and an admin who switched from single mode is asked to
  create one.
- **Settings > Delete account** removes the account and everything it owns
  once you type its handle. The admin's account goes last.
- API tokens, including those a device pairing mints, act as the account
  that created or approved them. `POMB_BOOTSTRAP_TOKEN` is a standing admin
  credential, so `--mode multi` refuses to start while it is set.

Backups copy the whole database file, so whoever runs the server can read
every account's data. A deleted or pruned board stays in the backups taken
before its deletion until they are rotated out.

## Backups

Pomb can copy its database file on a schedule. **Backups are off by default.**
Turn them on under **Settings > Backups**, which holds three settings, stored
in the database:

- Back up automatically: on or off (default: off).
- Hours between backups: 1 to 168 (default: 24).
- Backups to keep: 1 to 90 (default: 7).

A backup is `pomb-<UTC time>.redb`, for example `pomb-20261025T143000Z.redb`,
in the backup directory (`--backup-dir`). It is a consistent snapshot: Pomb
holds a write transaction while it copies the file, so writes wait for the copy
and the copy holds the last committed state. After a successful backup the
oldest `pomb-*.redb` files beyond the number to keep are deleted. Pomb touches
no other file in the directory, and a failed backup deletes nothing. The pane
also has a **Back up now** button, shows the result of the last backup (or its
error), and lists the kept files.

To restore:

1. Stop Pomb.
2. Copy the backup over the database file (`--database-path`).
3. Start Pomb.

Pomb opens every backup once before it keeps the file, so a copy that does not open counts as a failed backup and no older backup is deleted. A backup from a newer Pomb than the one you start is refused.

## Prebuilt binaries

Every push to `main` and every `v*` tag publishes a static x86_64 Linux
binary to GitHub Releases.

The newest `main` build is the `nightly` prerelease. Its release notes name the commit.

```sh
curl -LO https://github.com/chipsenkbeil/pomb/releases/download/nightly/pomb-x86_64-unknown-linux-musl
curl -LO https://github.com/chipsenkbeil/pomb/releases/download/nightly/pomb-x86_64-unknown-linux-musl.sha256
sha256sum -c pomb-x86_64-unknown-linux-musl.sha256
```

Once a `v*` tag has been released, the same two files are at
`https://github.com/chipsenkbeil/pomb/releases/latest/download/`. That URL
returns 404 until the first tagged release exists.

## Development

```sh
cargo run
```

The toolchain is pinned in `rust-toolchain.toml`. `dev lint`, `dev test`,
`dev fmt` and `dev build` run the checks (see `AGENTS.md`). Release builds
embed the web assets, so the installed binary needs nothing next to it.

Pomb serves htmx and SortableJS from `static/vendor/` instead of a CDN. They
are the published minified files, and these are their SHA-256 sums:

| File | Version | SHA-256 |
|---|---|---|
| `static/vendor/htmx-1.9.12.min.js` | htmx 1.9.12 | `449317ade7881e949510db614991e195c3a099c4c791c24dacec55f9f4a2a452` |
| `static/vendor/sortable-1.15.2.min.js` | SortableJS 1.15.2 | `ca68430703c4f5960e90735867c6e94d29b5a3de37107d8100e5a301007e9e6e` |

## Using it

- Drag cards between columns and swimlanes; dropping into **Done** stamps the
  completion date (grouped under Today / Yesterday / date), dragging out
  clears it. The **In progress** header turns amber at its WIP limit of 3.
- Columns and swimlanes are fully customizable at runtime: add, rename,
  reorder, delete, and set/clear WIP limits from the board.
- Click a card to open its detail modal: edit name/description, pick a
  pomodoro size (card color follows), see time spent / pomodori completed /
  interruptions, and log time manually. The rail on the right starts a
  Pomodoro or Stopwatch and jumps to the time log.
- The header timer pill tracks the active session across page loads: click it
  for the timer popup (today's sessions, add time, log, settings). Stopping a
  pomodoro early asks why; sessions under 20 seconds are discarded.
- The gear opens **Settings**: pomodoro/short-break/long-break lengths, ding
  and notification toggles, and the interruption-reason list.
- Card colors encode size: 1 pomodoro = yellow, 2 = green, 3 = blue,
  >3 = red.

## Agents

Agents call the JSON API with an `Authorization: Bearer pomb_...` token from
**Settings > API tokens**, or pair through the device flow and get a token
(shown once) when you approve the request. `/llms.txt` indexes the agent
documents, `/docs/api.md` is the API reference and `/api/v1/openapi.json` the
OpenAPI contract. Install the skill for Claude Code with this line (the Settings
page shows it for your server):

```sh
mkdir -p ~/.claude/skills/pomb && curl -fsS https://pomb.example.com/skill.md -o ~/.claude/skills/pomb/SKILL.md
```

## Roadmap

- `GET /api/boards/{id}/events?since=<seq>&limit=<n>`, paged like
  TimerLogPage, with `seq` and `task_id` added to TaskEvent.
