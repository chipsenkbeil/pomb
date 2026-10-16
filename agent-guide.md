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
`CHIPFLOW_API_TOKEN` env var on first headless setup; the env token works
before the admin account exists, and only the browser UI needs `/setup`
first). Token strings start
with `cf_` and are shown **once** at creation — store them in the agent's
environment (e.g. `CHIPFLOW_API_TOKEN`), never in prompts or chat logs.

Tokens carry scopes: `read` (GET requests) and `write` (`write` implies
`read`; POST/PATCH/PUT/DELETE require `write`). A token with insufficient
scope gets `403 insufficient token scope`. An invalid or expired token gets
`401 invalid or expired API token`. Token management endpoints
(`POST/GET /api/v1/auth/tokens`, `DELETE /api/v1/auth/tokens/:id`) require
the browser session cookie — a token can never mint new tokens.

Browser login (`POST /login`) sets the session cookie with the `Secure`
attribute only when the request arrived over HTTPS (e.g. behind a
TLS-terminating proxy). Over plain HTTP the cookie has no `Secure`
attribute, so cookie auth works on local dev servers.

## Conventions

- JSON request/response bodies throughout. Send `Content-Type: application/json`.
- Timestamps are RFC3339 strings: `created_at`, `completed_at`, `due_at`
  and `started_at` on tasks, comments, attachments, history and time
  entries. Two places use Unix seconds (UTC) instead: `started_at` in
  `GET /api/timer/status`, and the API token `created_at`, `last_used_at`
  and `expires_at`.
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
# Default response is the rendered card HTML fragment (the UI appends it).
# Send 'Accept: application/json' for a 201 JSON body instead:
# {"id":"<task-uuid>","name":"...","column_id":"...","swimlane_id":"..."}.
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -d '{"name":"Write release notes","column_id":"<column-uuid>"}' \
  "$BASE/api/tasks"
# estimate_hours: optional hour-based time estimate (KanbanFlow parity),
# e.g. {"estimate_hours":4.0}; the pomodoro size is derived from it unless
# size is also given.
# due_at: optional due date/time at creation, same formats as the PATCH
# below; an unparseable value is a 400 and creates no task.
# due_repeat: optional repeat text, e.g. "every week".
```

### Set a task's time estimate

```bash
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"estimate_hours":4.0}' "$BASE/api/tasks/<task-uuid>"
# estimate_hours: positive number of hours sets the estimate (the modal
# shows it as "4h"); exactly 0 clears it. Setting it also derives the
# pomodoro `size` from the configured pomodoro length unless `size` is
# given explicitly in the same request.
```

### Move a task (change column and/or swimlane)

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"column_id":"<done-column-uuid>","position":1.0}' \
  "$BASE/api/tasks/<task-uuid>/move"
# column_id and position are required. swimlane_id is optional: without it
# the task keeps its lane, or lands in the target board's first lane when
# column_id is on another board. position is the index among the target
# cell's other tasks (rounded, clamped to the ends). In a Done column it
# counts them in on-screen order (date groups, newest first) and the task
# lands beside the neighbor at that index from its own date group; with no
# such neighbor it keeps its place, or joins the top of its group when it
# comes from another cell. Moving into a Done column sets the task's
# completion time (the task modal shows "✓ Done (<date>)", the card shows
# no stamp); moving out clears it.
```

### Task detail, subtasks, and members

```bash
curl -s -H "$AUTH" "$BASE/api/tasks/<task-uuid>"
# {"id","member_ids":[],"grouping_date":null,"watched":false,
#  "subtasks":[{"id","name","done"}],"labels":[],"due_at":null,
#  "due_repeat":null,"due_done":false}
# The detail has no name, column, comments or attachments: names come from
# GET /api/tasks, comments from GET .../comments.

curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"Write tests"}' "$BASE/api/tasks/<task-uuid>/subtasks"
# Rename/toggle: PATCH .../subtasks/<sub-uuid> {"name":"...","done":true}
# (both fields optional). Remove: DELETE .../subtasks/<sub-uuid>.
# Reorder: PUT .../subtasks/order {"order":["<sub-uuid>",...]}.

curl -s -H "$AUTH" "$BASE/api/members"
# Board-member roster: [{"id","username"}] (single-admin: registered users).

curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"member_ids":["<user-uuid>"],"grouping_date":"2026-10-05"}' \
  "$BASE/api/tasks/<task-uuid>"
# member_ids replaces the assignment set. grouping_date takes a date
# written exactly as zero-padded YYYY-MM-DD after trimming surrounding
# spaces ("2026-1-5" or a timestamp is a 400), and null or "" clears it.
# When set, it picks the task's date group in the Done column in place of
# its completion date. Both fields optional.

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
# due_at: RFC3339 (stored as sent), or "YYYY-MM-DD HH:MM" /
# "YYYY-MM-DD HH:MM:SS" (space or T) in the server's local timezone; a bare
# "YYYY-MM-DD" means due at the end of that day (23:59:59). In the bare
# date and the local date-times every field but the year may drop its
# leading zero ("2026-1-5", "2026-10-20 8:5:7"). Seconds run 00-59 in
# every form, RFC3339 included, so a ":60" leap second is a 400.
# Anything else is a 400 and the request changes nothing. A local time
# that occurs twice at a daylight-saving fall-back resolves to the earlier
# instant; a local time that never occurs because a spring-forward skips
# it is a 400. Both use the server zone's exact transition second: in
# London, "2026-10-25 01:00" to "01:59:59" occur twice and "02:00" occurs
# once (02:00Z), while "2027-03-28 01:00" to "01:59:59" never occur.
# Empty string or null clears it. Re-sending
# the same instant in another format (e.g. ".000Z") is not a change and
# keeps due_done. A task is flagged overdue only once the full due
# timestamp has passed, never merely on the due date.
# due_repeat: free text, e.g. "every week"; empty string or null clears it.
# due_done: true/false marks the due date done (the card shows "(Done)";
# KanbanFlow parity for checking the due-date item).

curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"body":"Blocked on review"}' "$BASE/api/tasks/<task-uuid>/comments"
# Returns the created comment {"id","author","body","created_at"}.
# author is optional (a display name to attribute the comment to); it
# defaults to the authenticated username ("api-token" for API tokens).
curl -s -H "$AUTH" "$BASE/api/tasks/<task-uuid>/comments"
# JSON comment list, oldest first — no need to scrape the task modal HTML.
curl -s -X DELETE -H "$AUTH" "$BASE/api/tasks/<task-uuid>/comments/<comment-uuid>"

# Attachments (10 MiB cap on the decoded file, bytes stored in the
# database). The upload returns {"id","name","mime","size","uploaded_by",
# "created_at"}; keep the id, since no endpoint lists a task's attachments.
BASE64=$(base64 -w0 screenshot.png)
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"name\":\"screenshot.png\",\"mime\":\"image/png\",\"data\":\"$BASE64\"}" \
  "$BASE/api/tasks/<task-uuid>/attachments"
curl -s -H "$AUTH" \
  "$BASE/api/tasks/<task-uuid>/attachments/<attachment-uuid>/file" -o screenshot.png
curl -s -X DELETE -H "$AUTH" \
  "$BASE/api/tasks/<task-uuid>/attachments/<attachment-uuid>"
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
# {active, mode, mode_title, task_id, task_name, task_color, started_at, duration_secs}
```

### Stop the timer

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"completed":true}' \
  "$BASE/api/timer/stop"
# completed:true  = finished successfully (green "Successful Pomodoro").
# completed:false = interrupted; add "reason":"<why>" (red "Stopped Pomodoro
#   with reason '<why>'"). Sessions under 20 seconds are discarded.
# A break session can carry "activity_id":"<break-activity-uuid>" to attribute
# its time to a defined break activity (feeds the statistics Break activities tab).
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
`from`/`to`/`board_id` and return `by_reason`, `best_day` (null when no
pomodoro was logged in the range), `longest_streak`,
plus `break_activities` (per defined break activity: id, name, minutes in
range, today_minutes, daily_goal, daily_limit).

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
# Reorder with POST /api/columns/<uuid>/move {"position":2}.
# PATCH {"wip_limit":null} clears the limit; absent leaves it unchanged.
# A column with "is_done":true counts as a Done column.
# GET /api/boards/<board-uuid>/columns lists the board's columns as
# [{"id","name","wip_limit","is_done"}] (wip_limit is null when unset).

curl -s -H "$AUTH" "$BASE/api/boards/<board-uuid>/export-data"
# The board reports' shared dataset: {"board":{"id","name"},
# "columns":[{"id","name"}], "swimlanes":[{"id","name"}],
# "colors":[{"id","value","label"}], "users":[{"id","username"}],
# "tasks":[full task rows]}. The Export report page (/b/<board-uuid>/export)
# downloads this and renders Excel/CSV/XML/JSON client-side. The Task count
# (/b/<board-uuid>/task-count, KF-343), Throughput (/b/<board-uuid>/throughput,
# KF-344), Burndown (/b/<board-uuid>/burndown, KF-337), Cumulative flow
# (/b/<board-uuid>/cumulative-flow, KF-339), Cycle & lead time
# (/b/<board-uuid>/cycle-lead, KF-340), Due date performance
# (/b/<board-uuid>/due-date, KF-341), Board history
# (/b/<board-uuid>/board-history, KF-336), Calendar
# (/b/<board-uuid>/calendar, KF-338), Monte Carlo forecasting
# (/b/<board-uuid>/monte-carlo, KF-342), and Time estimate
# (/b/<board-uuid>/time-estimate, KF-345) report pages render from the same
# dataset client-side.
# Calendar iCal feed (KF-351): GET /api/boards/<board-uuid>/calendar-feed
# returns {"path":"/api/calendar-feed/<token>.ics"} — the token is a 32-byte
# bearer secret created on first call, revealed only here (never in page
# source). POST /api/boards/<board-uuid>/calendar-feed/reset rotates it.
# GET /api/calendar-feed/<token>.ics is unauthenticated by design (calendar
# clients can't do cookie auth) and serves text/calendar with one VEVENT per
# task due date (minimal fields; RRULE for recognized due_repeat cadences).
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

```bash
curl -s -H "$AUTH" -X PUT -H 'Content-Type: application/json' \
  -d '{"legend_visible":true}' "$BASE/api/boards/<board-uuid>/config"
```

Merges the given fields into the board's opaque config bag (e.g. the Color
legend Menu toggle persists `legend_visible` here, per board); responds with
the current config, e.g. `{"legend_visible":true}`.

KF-305: `{"archived":true}` archives the board (hidden from `GET /api/boards`
and the Boards dropdown unless `?include_archived=true`); `{"archived":false}`
unarchives. Archived boards are listed at `/boards/archived`.

Board footer semantics (KF-222): the color-legend footer renders only when
`legend_visible` is true — no footer markup at all when false. When on, it
shows one labeled segment per enabled board color in palette order, labeled
with each color's custom name. New boards default to false (KanbanFlow
parity); boards predating the toggle were backfilled to true.

## Notes for agents

- Prefer `GET /api/v1/openapi.json` over guessing at undocumented paths.
- The timer keeps server-side state: `started_at` is authoritative. The web
  UI counts overtime past the duration; stop whenever, and the logged
  minutes reflect actual elapsed time.
- Only a pomodoro stopped with `completed:true` counts toward the task's
  pomodoro total. A stopped pomodoro (`completed:false`) logs its time and
  records an interruption instead.
- `POST /api/timer/retarget {"task_id":"<uuid>"}` re-points a running timer
  at a different task without stopping it.
- Keep the token out of logs, files, and chat transcripts. If a token leaks,
  revoke it in Settings → API tokens and mint a new one.
