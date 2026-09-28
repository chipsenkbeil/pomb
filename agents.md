# ChipFlow Agent Guide

ChipFlow is a self-hosted kanban board with a pomodoro timer, time tracking,
and time logging. This page is the complete guide for programmatic access.

## Base URL and auth

The app is served from a single base URL (whatever host/port it runs on),
e.g. `http://greenbox:8080` or `http://localhost:8080`. All API paths below
are relative to it.

Authenticate every request with a Bearer API token:

```
Authorization: Bearer cf_<token>
```

Tokens are created in the web UI under **Settings → API tokens** (or via the
`CHIPFLOW_API_TOKEN` env var on first headless setup). Token strings start
with `cf_` and are shown **once** at creation — store them in the agent's
environment (e.g. `CHIPFLOW_API_TOKEN`), never in prompts or chat logs.

Tokens carry scopes: `read` (GET requests) and `write` (`write` implies
`read`; POST/PATCH/PUT/DELETE require `write`). A token with insufficient
scope gets `403 insufficient token scope`. An invalid or expired token gets
`401 invalid or expired API token`. Token management endpoints
(`POST/GET /api/v1/auth/tokens`, `DELETE /api/v1/auth/tokens/:id`) require
the browser session cookie — a token can never mint new tokens.

## Conventions

- JSON request/response bodies throughout. Send `Content-Type: application/json`.
- Timestamps are Unix seconds (UTC).
- IDs are UUID strings.
- Errors are plain-text bodies with an appropriate 4xx/5xx status, e.g.
  `400 token name must be 1-80 characters`. There is no error envelope.
- Durations are truncated to whole minutes (never rounded up).

## Discovery

- `GET /agents.md` — this guide.
- `GET /agents/skill.md` — this guide in Agent Skills format (installable).
- `GET /api/v1/openapi.json` — machine-readable contract for every endpoint.
- `GET /.well-known/agents.json` — pointer to the above.

## Core workflows

All examples assume:

```bash
BASE=http://localhost:8080
AUTH="Authorization: Bearer $CHIPFLOW_API_TOKEN"
```

### List the board's tasks

```bash
curl -s -H "$AUTH" "$BASE/api/tasks" | head -c 600
# Board context (columns, swimlanes) comes from the HTML at $BASE/ if needed.
```

### Create a task

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"Write release notes","column_id":"<column-uuid>","size":2}' \
  "$BASE/api/tasks"
# size: 1..4 pomodori (4 = ">3"). column_id is required; swimlane_id is
# optional (defaults to the board's first swimlane).
```

### Move a task (change column and/or swimlane)

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"column_id":"<done-column-uuid>","position":1.0}' \
  "$BASE/api/tasks/<task-uuid>/move"
# column_id and position (sort order within the column) are required;
# swimlane_id is optional. Moving into a Done column stamps the card
# "✓ <date>"; moving out clears it.
```

### Start a pomodoro on a task

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"mode":"pomodoro","task_id":"<task-uuid>"}' \
  "$BASE/api/timer/start"
# mode: pomodoro | stopwatch | short_break | long_break.
# Durations come from Settings (default 25/5/15). Only one timer runs at a
# time; starting another replaces it (the replaced session is logged).
```

### Check timer status

```bash
curl -s -H "$AUTH" "$BASE/api/timer/status"
# {active, mode, mode_title, task_id, task_name, started_at, duration_secs}
```

### Stop the timer

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"completed":true}' \
  "$BASE/api/timer/stop"
# completed:true  = finished successfully (green "Successful Pomodoro").
# completed:false = interrupted; add "reason":"<why>" (red "Stopped Pomodoro
#   with reason '<why>'"). Sessions under 20 seconds are discarded.
```

### Log time manually

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"task_id":"<task-uuid>","date":"2026-09-28","from":"09:00","to":"10:30","note":"Deep work"}' \
  "$BASE/api/time/manual"
# date: YYYY-MM-DD. from/to: HH:MM 24h. Rejects times in the future and
# zero/negative durations. Manual entries affect "Time spent", not pomodori.
```

### Read the timer log / statistics

```bash
curl -s -H "$AUTH" "$BASE/api/timer/log?limit=50&offset=0"
curl -s -H "$AUTH" "$BASE/api/timer/log?task_id=<task-uuid>"
curl -s -H "$AUTH" "$BASE/api/timer/time-spent?from=2026-09-01&to=2026-09-30"
curl -s -H "$AUTH" "$BASE/api/timer/statistics"
```

### Columns and swimlanes

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"Review","wip_limit":3}' "$BASE/api/columns"
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"In review"}' "$BASE/api/columns/<uuid>"
curl -s -X DELETE -H "$AUTH" "$BASE/api/columns/<uuid>"
# Same shapes under /api/swimlanes. Deleting a non-empty column/swimlane
# returns 400. Reorder with POST /api/columns/<uuid>/move {"to_index":2}.
# A column with "is_done":true counts as a Done column.
```

## Notes for agents

- Prefer `GET /api/v1/openapi.json` over guessing at undocumented paths.
- The timer keeps server-side state: `started_at` is authoritative. The web
  UI counts overtime past the duration; stop whenever, and the logged
  minutes reflect actual elapsed time.
- Stopped pomodori still count toward the task's pomodoro total (and record
  an interruption); only `completed:true` avoids the interruption count.
- `POST /api/timer/retarget {"task_id":"<uuid>"}` re-points a running timer
  at a different task without stopping it.
- Keep the token out of logs, files, and chat transcripts. If a token leaks,
  revoke it in Settings → API tokens and mint a new one.
