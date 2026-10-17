---
name: chipflow
description: Operate a ChipFlow kanban + pomodoro server over its REST API. Use when the user asks to manage ChipFlow tasks, timers, time entries, columns, swimlanes, boards, board templates, or per-board task colors, or points you at a ChipFlow base URL with an API token.
---

# ChipFlow Skill

ChipFlow is a self-hosted kanban board with a pomodoro timer and time
tracking, exposed as a JSON REST API on a single base URL.

## Setup

The server URL and token come from the environment:

- `CHIPFLOW_URL` — base URL, e.g. `http://greenbox:8080`
- `CHIPFLOW_API_TOKEN` — API token (starts with `cf_`)

Authenticate every request:

```
Authorization: Bearer $CHIPFLOW_API_TOKEN
```

If either variable is missing, ask the user for the server URL and token
(the token is created in ChipFlow's web UI under Settings → API tokens).
Never print or log the token.

Token scopes: `read` covers GET requests; `write` (which implies `read`)
is required for POST/PATCH/PUT/DELETE. Insufficient scope → `403`;
bad/expired token → `401`.

## Quick reference

```bash
AUTH="Authorization: Bearer $CHIPFLOW_API_TOKEN"
# Tasks
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/tasks"
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"<name>","column_id":"<uuid>","size":2}' \
  "$CHIPFLOW_URL/api/tasks"
# With 'Accept: application/json' this returns 201
# {"id":"<task>","name":"...","column_id":"...","swimlane_id":"..."};
# the default response is the rendered card HTML fragment.
# Optional: "due_at" (surrounding whitespace trimmed; RFC3339 stored as
# sent, or "YYYY-MM-DD HH:MM[:SS]" in server-local time with one space or
# uppercase T before the time; seconds 00-59 in every form; bare
# date means end of day; four-digit unsigned year; other leading zeros
# optional except in RFC3339; a local time repeated by a DST
# fall-back takes the earlier instant; a local time skipped by a DST
# spring-forward or any other format is a 400) and "due_repeat"
# ("every week") can be set at creation.
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"column_id":"<uuid>","position":1.0}' \
  "$CHIPFLOW_URL/api/tasks/<task>/move"
# Optional "swimlane_id"; without it the task keeps its lane, or takes the
# target board's first lane when column_id is on another board.
# position: index among the cell's other tasks; in a Done column it counts
# on-screen order (date groups, newest first). See /agents.md.
# Timer (one runs at a time; server tracks started_at authoritatively)
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"mode":"pomodoro","task_id":"<task>"}' \
  "$CHIPFLOW_URL/api/timer/start"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/status"
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"completed":true}' "$CHIPFLOW_URL/api/timer/stop"
# Manual time (affects "Time spent", not pomodori)
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"task_id":"<task>","date":"2026-09-28","from":"09:00","to":"10:30","note":"..."}' \
  "$CHIPFLOW_URL/api/time/manual"
# Delete a time entry: curl -s -X DELETE -H "$AUTH" "$CHIPFLOW_URL/api/time/entries/<entry>"
# Labels, due dates, comments, attachments (KanbanFlow parity)
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/boards/<board>/labels"
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"labels":["a","b"],"due_at":"2026-10-05T17:00:00Z","due_repeat":"every week","due_done":true}' \
  "$CHIPFLOW_URL/api/tasks/<task>"
# due_done: true marks the due date done (card shows "(Done)").
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"body":"..."}' "$CHIPFLOW_URL/api/tasks/<task>/comments"
# author is optional (display name attribution; defaults to the
# authenticated username). GET .../comments returns the JSON comment
# list, oldest first.
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/tasks/<task>/comments"
curl -s -X DELETE -H "$AUTH" "$CHIPFLOW_URL/api/tasks/<task>/comments/<comment>"
# Attachment upload (base64 data, 10 MiB cap on the decoded file) returns
# its metadata with the id; download at .../attachments/<attachment>/file,
# DELETE .../attachments/<attachment> removes it. No endpoint lists them.
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"file.png","mime":"image/png","data":"<base64>"}' \
  "$CHIPFLOW_URL/api/tasks/<task>/attachments"
# Log and stats
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/log?limit=50"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/log?board_id=<board>&entry_type=pomodoro&from=2026-09-01&to=2026-09-30"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/time-spent?from=2026-09-01&to=2026-09-30"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/time-spent?from=2026-09-01&to=2026-09-30&color_id=<color>"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/statistics"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/statistics?from=2026-09-01&to=2026-09-30&board_id=<board>"
# Settings (partial PUT; ticking_mode never|timer_start|always; alarm_sound
# bell|chime|beeps|blip|glass|microwave|egg_timer|grandpa_clock|melodic;
# break_activities [{id,name,description,daily_goal,daily_limit|null}];
# favorite_boards [<board-uuid>, ...])
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/settings"
curl -s -X PUT -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"alarm_sound":"chime","alarm_volume":80}' "$CHIPFLOW_URL/api/settings"
# Boards, templates, colors
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/boards"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/boards/<board>/columns"
# [{"id","name","wip_limit","is_done"}]; wip_limit is null when unset.
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/boards/<board>/export-data"
# Board reports' shared dataset: {"board","columns","swimlanes","colors",
# "users","tasks"}. The Export, Task count (/b/<board>/task-count),
# Throughput (/b/<board>/throughput), Burndown (/b/<board>/burndown),
# Cumulative flow (/b/<board>/cumulative-flow), Cycle & lead time
# (/b/<board>/cycle-lead), Due date performance (/b/<board>/due-date),
# Board history (/b/<board>/board-history), Calendar
# (/b/<board>/calendar), Monte Carlo forecasting
# (/b/<board>/monte-carlo), and Time estimate (/b/<board>/time-estimate)
# report pages render client-side from this.
# Calendar iCal feed (KF-351): GET /api/boards/<board>/calendar-feed ->
# {"path":"/api/calendar-feed/<token>.ics"} (bearer token created on first
# call, revealed only here); POST .../calendar-feed/reset rotates it;
# GET /api/calendar-feed/<token>.ics serves the text/calendar feed
# unauthenticated (one VEVENT per due date; RRULE for recognized repeats).
curl -s -X DELETE -H "$AUTH" "$CHIPFLOW_URL/api/boards/<board>"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/templates"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/boards/<board>/colors"
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"label":"Deep work","is_default":true}' \
  "$CHIPFLOW_URL/api/boards/<board>/colors/<color>"
# Copy a palette: POST /api/boards/<board>/colors/copy-from
# {"source_board_id":"<other-board>"} -> {"count":N}; tasks keep colors by value.
# Per-board UI settings: PUT /api/boards/<board>/config {"legend_visible":true}
# merges into the board config bag -> {"legend_visible":true}
# KF-305: {"archived":true} archives the board (hidden from default lists).
# Task colors: POST /api/tasks {"color_id":"<color>"} assigns;
# PATCH /api/tasks/<task> {"color_id":""} clears to size-based coloring.
# Time estimates: POST/PATCH /api/tasks {"estimate_hours":4.0} sets an
# hour-based estimate (KanbanFlow parity); {"estimate_hours":0} clears it.
# Setting it derives the pomodoro size unless "size" is given explicitly.
# Task detail / subtasks / members
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/tasks/<task>"
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"<subtask>"}' "$CHIPFLOW_URL/api/tasks/<task>/subtasks"
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"done":true}' "$CHIPFLOW_URL/api/tasks/<task>/subtasks/<subtask>"
# Reorder: PUT "$CHIPFLOW_URL/api/tasks/<task>/subtasks/order" -d '{"order":["<subtask>",...]}'
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/members"
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"member_ids":["<user>"],"grouping_date":"2026-10-05"}' \
  "$CHIPFLOW_URL/api/tasks/<task>"
# grouping_date (zero-padded YYYY-MM-DD, anything else is a 400; null or
# "" clears) picks the task's Done-column date group in place of its
# completion date.
# Watch: {"watched":true} marks the task as watched (GET /api/tasks/<task>
# returns "watched"); currently persisted state only, no notifications.
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"watched":true}' "$CHIPFLOW_URL/api/tasks/<task>/watch"
```

## Full documentation

- `$CHIPFLOW_URL/agents.md` — the complete agent guide with every workflow.
- `$CHIPFLOW_URL/api/v1/openapi.json` — machine-readable endpoint contract.

## Notes

- Errors are plain-text bodies with 4xx/5xx statuses (no envelope).
- Timestamps are RFC3339 strings, except Unix seconds (UTC) for the timer
  status `started_at` and API token times; durations truncate to whole
  minutes.
- Only `completed:true` pomodori count toward the task's pomodoro total. A
  stopped pomodoro (`completed:false`) records an interruption instead (pass
  `"reason":"<why>"` to label it).
- Sessions under 20 seconds are discarded on stop.
- `POST /api/timer/retarget {"task_id":"<uuid>"}` re-points a running timer
  without stopping it.
