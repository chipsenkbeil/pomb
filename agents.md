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
- `GET /api/v1/version` — build version and commit SHA (public, no auth).
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

### Task detail, subtasks, and members

```bash
curl -s -H "$AUTH" "$BASE/api/tasks/<task-uuid>"
# {"id","member_ids":[],"grouping_date":null,"watched":false,"subtasks":[{"id","name","done"}]}

curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"Write tests"}' "$BASE/api/tasks/<task-uuid>/subtasks"
# Rename/toggle: PATCH .../subtasks/<sub-uuid> {"name":"...","done":true}
# (both fields optional). Remove: DELETE .../subtasks/<sub-uuid>.

curl -s -H "$AUTH" "$BASE/api/members"
# Board-member roster: [{"id","username"}] (single-admin: registered users).

curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"member_ids":["<user-uuid>"],"grouping_date":"2026-10-05"}' \
  "$BASE/api/tasks/<task-uuid>"
# member_ids replaces the assignment set; grouping_date accepts a
# YYYY-MM-DD date or null to clear. Both fields optional.

curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"watched":true}' "$BASE/api/tasks/<task-uuid>/watch"
# Task "Watch" toggle (KanbanFlow parity): sets the task's persisted watched
# flag, shown in the modal More menu. Returns {"watched":true}.
```

### Labels, due dates, comments, and attachments

```bash
curl -s -H "$AUTH" "$BASE/api/boards/<board-uuid>/labels"
# Label suggestions: distinct labels already used on the board's tasks.

curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"labels":["frontend","urgent"]}' "$BASE/api/tasks/<task-uuid>"
# Replaces the task's label set. GET /api/tasks/<task-uuid> returns it as
# "labels".

curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"due_at":"2026-10-05T17:00:00Z","due_repeat":"every week"}' \
  "$BASE/api/tasks/<task-uuid>"
# due_at: RFC3339 (or "YYYY-MM-DD HH:MM"); empty string or null clears it.
# due_repeat: free text, e.g. "every week"; empty string or null clears it.

curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"body":"Blocked on review"}' "$BASE/api/tasks/<task-uuid>/comments"
# Returns the created comment {"id","body","created_by","created_at"}.
curl -s -X DELETE -H "$AUTH" "$BASE/api/tasks/<task-uuid>/comments/<comment-uuid>"

# Attachments (10 MiB cap, bytes stored in the database):
BASE64=$(base64 -w0 screenshot.png)
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"name\":\"screenshot.png\",\"mime\":\"image/png\",\"data\":\"$BASE64\"}" \
  "$BASE/api/tasks/<task-uuid>/attachments"
curl -s -H "$AUTH" \
  "$BASE/api/tasks/<task-uuid>/attachments/<attachment-uuid>/file" -o screenshot.png
curl -s -X DELETE -H "$AUTH" \
  "$BASE/api/tasks/<task-uuid>/attachments/<attachment-uuid>"
# GET /api/tasks/<task-uuid> lists attachment metadata under "attachments";
# each entry has a "download_url" (the /file path above).
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
# "labels": ["a","b"] tags the entry (optional).
```

### Delete a time entry

```bash
curl -s -X DELETE -H "$AUTH" "$BASE/api/time/entries/<entry-uuid>"
# Log entries carry "labels" and "created_by" alongside the usual fields.
```

### Read the timer log / statistics

```bash
curl -s -H "$AUTH" "$BASE/api/timer/log?limit=50&offset=0"
curl -s -H "$AUTH" "$BASE/api/timer/log?task_id=<task-uuid>"
curl -s -H "$AUTH" "$BASE/api/timer/log?board_id=<board-uuid>&entry_type=pomodoro&from=2026-09-01&to=2026-09-30"
curl -s -H "$AUTH" "$BASE/api/timer/time-spent?from=2026-09-01&to=2026-09-30"
curl -s -H "$AUTH" "$BASE/api/timer/time-spent?from=2026-09-01&to=2026-09-30&board_id=<board-uuid>"
curl -s -H "$AUTH" "$BASE/api/timer/time-spent?from=2026-09-01&to=2026-09-30&color_id=<color-uuid>"
curl -s -H "$AUTH" "$BASE/api/timer/statistics"
curl -s -H "$AUTH" "$BASE/api/timer/statistics?from=2026-09-01&to=2026-09-30&board_id=<board-uuid>"
```
Log filters: `task_id`, `board_id`, `entry_type` (pomodoro|stopwatch|manual),
`from`/`to` (YYYY-MM-DD). Log entries carry `board_id`, `board_name`,
`day_key`, `badge_code`, `badge_title`, `time_range`. Statistics accept
`from`/`to`/`board_id` and return `by_reason`, `best_day`, `longest_streak`.

### Columns and swimlanes

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"Review","wip_limit":3}' "$BASE/api/columns"
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"In review"}' "$BASE/api/columns/<uuid>"
curl -s -X DELETE -H "$AUTH" "$BASE/api/columns/<uuid>"
# Same shapes under /api/swimlanes. Deleting a non-empty column/swimlane
# returns 400, as does deleting the last swimlane on a board (every board
# keeps at least one so the column "+" add-task buttons keep working).
# Reorder with POST /api/columns/<uuid>/move {"to_index":2}.
# A column with "is_done":true counts as a Done column.
```

### Boards and board templates

```bash
curl -s -H "$AUTH" "$BASE/api/boards"
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"Side project","template_id":"<template-uuid>"}' "$BASE/api/boards"
# template_id is optional; omit it for a blank board (seeds the 10-color
# palette, one "To-do" column, and one "Default" swimlane). The built-in
# "Pomodoro board" template seeds the 10-color palette, 4 columns, and
# 2 swimlanes. GET /boards/new renders the new-board page.

curl -s -H "$AUTH" "$BASE/api/templates"
curl -s -X DELETE -H "$AUTH" "$BASE/api/templates/<uuid>"
# The built-in template cannot be deleted (400).
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"My recipe","description":"..."}' \
  "$BASE/api/boards/<board-uuid>/save-as-template"
# Captures the board's colors, columns, swimlanes, and tasks as a reusable template.
```

### Per-board task colors

Each board has its own color palette (10 standard KanbanFlow colors).
Tasks reference a color by its color-slot id; new tasks default to the
board's default color unless `color_id` (or legacy `size`) is given.

```bash
curl -s -H "$AUTH" "$BASE/api/boards/<board-uuid>/colors"
# [{id, value ("yellow"...), label, description, enabled, is_default,
#   sort_order, bg, border, light}]

curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"label":"Deep work","enabled":true,"is_default":true}' \
  "$BASE/api/boards/<board-uuid>/colors/<color-uuid>"
# Rename (max 50 chars), set the legend tooltip ("description"),
# enable/disable the picker entry, change the default, or reorder with
# {"sort_order":2}. All fields optional; applied atomically.

curl -s -X DELETE -H "$AUTH" "$BASE/api/boards/<board-uuid>/colors/<color-uuid>"
# 400 when the color is the board default or still used by tasks.

# Creating a task with an explicit color:
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"Write docs","column_id":"<uuid>","color_id":"<color-uuid>"}' \
  "$BASE/api/tasks"
# PATCH /api/tasks/<uuid> {"color_id":"<color-uuid>"} reassigns;
# {"color_id":""} clears back to the legacy size-based coloring.

# Copy another board's palette onto this board (replaces the whole
# palette; tasks keep their colors by standard color value):
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"source_board_id":"<other-board-uuid>"}' \
  "$BASE/api/boards/<board-uuid>/colors/copy-from"
# {"count":10}. 400 when the source board is unknown or equals the target.
```

## Settings

```bash
curl -s -H "$AUTH" "$BASE/api/settings"
curl -s -H "$AUTH" -X PUT -H "Content-Type: application/json" \
  -d '{"alarm_sound":"chime","alarm_volume":80,"ticking_mode":"always"}' \
  "$BASE/api/settings"
```

Partial updates: only the fields present are changed. `ticking_mode` is
`never`|`timer_start`|`always`; `alarm_sound` is one of `bell`, `chime`,
`beeps`, `blip`, `glass`, `microwave`, `egg_timer`, `grandpa_clock`,
`melodic`; volumes are 0–100; `sounds_enabled` and `pip_enabled` are booleans.
`break_activities` replaces the whole break-activity list — each entry is
`{id, name, description, daily_goal (1–100), daily_limit|null}`; blank ids
are regenerated, blank names are dropped. `favorite_boards` replaces the
whole Favorites list — unknown or duplicate board ids are dropped.

## Boards

```bash
curl -s -H "$AUTH" -X DELETE "$BASE/api/boards/<board-uuid>"
```

Deletes a board and all its columns, swimlanes, tasks, time entries, and
colors. Refuses (400) when it is the last remaining board.

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
