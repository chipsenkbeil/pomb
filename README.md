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
- `--host` / `HOST`: address to listen on (default: `127.0.0.1`).
- `--port` / `PORT`: HTTP port (default: `3000`).
- `--database-path` / `DATABASE_PATH`: database file
  (default: `~/.local/share/pomb/pomb.redb`, honoring `XDG_DATA_HOME`).
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

There is no recovery code. Instead:

1. Restart with `--auth none` on a loopback `--host`, keeping
   `POMB_PUBLIC_URL` set.
2. Open **Settings > Passkeys** at the public URL and add a passkey. Passkeys
   are bound to the public URL's host, so the page must be opened there and
   not at `localhost`. Until step 3, anyone who reaches that URL acts as you,
   so keep this short.
3. Restart with `--auth passkey`.

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
