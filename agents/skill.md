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
# Log and stats
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/log?limit=50"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/log?board_id=<board>&entry_type=pomodoro&from=2026-09-01&to=2026-09-30"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/time-spent?from=2026-09-01&to=2026-09-30"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/statistics"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/statistics?from=2026-09-01&to=2026-09-30&board_id=<board>"
# Boards, templates, colors
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/boards"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/templates"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/boards/<board>/colors"
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"label":"Deep work","is_default":true}' \
  "$CHIPFLOW_URL/api/boards/<board>/colors/<color>"
# Task colors: POST /api/tasks {"color_id":"<color>"} assigns;
# PATCH /api/tasks/<task> {"color_id":""} clears to size-based coloring.
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
