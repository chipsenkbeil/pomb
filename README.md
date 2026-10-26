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
  serves the admin plus the people it invites (see Accounts below).
- `--host` / `HOST`: address to listen on (default: `127.0.0.1`).
- `--port` / `PORT`: HTTP port (default: `3000`).
- `--database-path` / `DATABASE_PATH`: database file
  (default: `~/.local/share/pomb/pomb.redb`, honoring `XDG_DATA_HOME`).
- `--backup-dir` / `POMB_BACKUP_DIR`: where backups are written
  (default: a `backups` directory beside the database file).
- `--public-url` / `POMB_PUBLIC_URL`: externally visible base URL, such as
  `https://pomb.example.com` (scheme, host and optional port, no path).
  It sets the device-pairing `verification_url` and the OpenAPI `servers`
  entry. Unset, the request `Host` header is used. `--auth passkey` needs it.

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
  the proxy is trusted.
- `--auth passkey`: you sign in with a passkey. `POMB_PUBLIC_URL` is required,
  its host (a DNS name, not an IP address) is the passkey relying party, and
  Pomb may listen on any address, for example behind a reverse proxy. While
  no passkey exists, each start prints a one-time setup code to the log
  (`pomb: setup code ABCD-EFGH ...`). The setup page asks for that code and a
  username and registers your first passkey, and the code then stops
  working. **Settings > Passkeys** lists your passkeys and adds or removes
  them, though the last one always stays.

A database from a release with passwords opens fine. The password is
dropped, and under `--auth passkey` the setup page registers the existing
user's first passkey with the setup code from the log. The upgrade also
ends every browser session, since each was opened with a password.

### Lost every passkey

Under `--mode multi`, open `/recover` and enter your handle and recovery code
(see Accounts). In single mode there is no recovery code. Instead:

1. Restart with `--auth none` on a loopback `--host`, keeping
   `POMB_PUBLIC_URL` set.
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
- The admin creates single-use invite links under **Settings > Invites**,
  each valid for 1 to 30 days (7 by default), and can revoke unused ones.
  Opening a link shows the sign-up page, where the invited person picks a
  handle (3 to 32 of `a-z`, `0-9`, `_` and `-`) and registers a passkey.
- Every account gets one recovery code, shown once at sign-up and stored only
  as a hash. With its handle it registers a new passkey at `/recover`, which
  also signs out every browser of that account and shows a new code.
  **Settings > Recovery code** replaces it. On first setup in multi mode the
  admin gets one too, and an admin who switched from single mode is asked to
  create one.
- **Settings > Delete account** removes the account and everything it owns
  once you type its handle. The admin's account goes last.
- API tokens, including those a device pairing mints, act as the account
  that created or approved them. `POMB_BOOTSTRAP_TOKEN` acts as the admin.

Backups copy the whole database file, so whoever runs the server can read
every account's data.

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
