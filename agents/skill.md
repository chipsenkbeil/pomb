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
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"column_id":"<uuid>","position":1.0}' \
  "$CHIPFLOW_URL/api/tasks/<task>/move"
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
  -d '{"labels":["a","b"],"due_at":"2026-10-05T17:00:00Z","due_repeat":"every week"}' \
  "$CHIPFLOW_URL/api/tasks/<task>"
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"body":"..."}' "$CHIPFLOW_URL/api/tasks/<task>/comments"
curl -s -X DELETE -H "$AUTH" "$CHIPFLOW_URL/api/tasks/<task>/comments/<comment>"
# Attachment upload (base64 data, 10 MiB cap); download at
# .../attachments/<attachment>/file; DELETE removes it.
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
# Task colors: POST /api/tasks {"color_id":"<color>"} assigns;
# PATCH /api/tasks/<task> {"color_id":""} clears to size-based coloring.
# Task detail / subtasks / members
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/tasks/<task>"
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"<subtask>"}' "$CHIPFLOW_URL/api/tasks/<task>/subtasks"
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"done":true}' "$CHIPFLOW_URL/api/tasks/<task>/subtasks/<subtask>"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/members"
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"member_ids":["<user>"],"grouping_date":"2026-10-05"}' \
  "$CHIPFLOW_URL/api/tasks/<task>"
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
- Timestamps are Unix seconds (UTC); durations truncate to whole minutes.
- Stopped pomodori (`completed:false`) still count toward the task's pomodoro
  total and record an interruption — pass `"reason":"<why>"` to label it.
- Sessions under 20 seconds are discarded on stop.
- `POST /api/timer/retarget {"task_id":"<uuid>"}` re-points a running timer
  without stopping it.
