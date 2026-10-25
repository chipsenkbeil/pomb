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

Open http://localhost:3000 — the setup page creates your admin account on
first run (the argon2 hash is stored in the database; the password itself
is never kept).

Configuration: flags or the matching environment variables. Everything has
a sane default, so bare `pomb` just works.

- `--port` / `PORT` — HTTP port (default: `3000`).
- `--database-path` / `DATABASE_PATH` — database file
  (default: `~/.local/share/pomb/pomb.redb`, honoring `XDG_DATA_HOME`).
- `--public-url` / `POMB_PUBLIC_URL` — externally visible base URL, such as
  `https://pomb.example.com` (scheme, host and optional port, no path).
  It sets the device-pairing `verification_url` and the OpenAPI `servers`
  entry. Unset, the request `Host` header is used.

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
