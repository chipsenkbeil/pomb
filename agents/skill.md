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

Without a token, pair instead (RFC 8628 device flow): `POST
$CHIPFLOW_URL/api/v1/auth/device` with `{"client_name":"<you>","scopes":["read","write"]}`
(no auth) returns `device_code`, `user_code`, `verification_url`,
`interval_seconds` and `expires_in_seconds`. Ask the user to open the URL
and approve the code, then poll `POST $CHIPFLOW_URL/api/v1/auth/device/token`
with `{"device_code":"..."}` no faster than `interval_seconds`: 428
`authorization_pending`, 429 `slow_down` (wait longer), 403 `denied`, 410
`expired`, 400 `invalid_grant`; 200 returns `{"token","name","scopes"}` once.

Token scopes: `read` covers GET requests; `write` (which implies `read`)
is required for POST/PATCH/PUT/DELETE. Insufficient scope → `403`;
bad/expired token → `401`. Browser-cookie writes must be same-origin
(cross-origin ones get `403 cross-site request refused`); Bearer
requests are unaffected.

## Quick reference

```bash
AUTH="Authorization: Bearer $CHIPFLOW_API_TOKEN"
# Tasks
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/boards/<board>/tasks"
# [{"id","name","column_id","swimlane_id","position","labels","member_ids",
#   "updated_at","metadata","blocked","claim","fields"}] in board order.
# Filters (all must match): column_id, swimlane_id, label (repeatable; the task must carry every one), since
# (RFC3339, updated_at after it; use Z or encode "+" as %2B; updated_at
# also changes when a task's blocked value changes), ready
# (true: not blocked; false: blocked), claimed (true: live claim).
# 404 "Board not found".
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/boards/<board>/tasks?label=agent&since=2026-10-16T18:00:00Z"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/tasks?board_id=<board>"
# All tasks as {"id","name","board_id"} items; board_id= keeps one board's
# tasks (empty value: no filter; 404 "Board not found" for an unknown board).
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -d '{"name":"<name>","column_id":"<uuid>","description":"...","labels":["agent"]}' \
  "$CHIPFLOW_URL/api/tasks"
# With 'Accept: application/json' this returns 201 and the task detail
# (the GET /api/tasks/<task> shape); without it, the rendered card HTML.
# PATCH /api/tasks/<task> with 'Accept: application/json' returns 200 and
# the task detail the same way.
# Optional: "due_at" (surrounding whitespace trimmed; RFC3339 stored as
# sent, or "YYYY-MM-DD HH:MM[:SS]" in server-local time with one space or
# uppercase T before the time; seconds 00-59 in every form; bare
# date takes the board's due time (17:00 unless PUT .../config sets
# "due_time":"HH:MM"); four-digit unsigned year; other leading zeros
# optional except in RFC3339; a local time repeated by a DST
# fall-back takes the earlier instant; a local time skipped by a DST
# spring-forward or any other format is a 400) and "due_repeat"
# ("every week", kept only with a due_at) can be set at creation.
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"column_id":"<uuid>","position":1.0}' \
  "$CHIPFLOW_URL/api/tasks/<task>/move"
# Optional "swimlane_id"; without it the task keeps its lane, or takes the
# target board's first lane when column_id is on another board; its color
# becomes the target board's color of the same value, else that board's default.
# position: index among the cell's other tasks; in a Done column it counts
# on-screen order (date groups: Today first, then future and past days newest
# first, undated last). See /agents.md.
# 409 "column WIP limit reached (n/limit)" when another column is full;
# ?force=true moves anyway.
# Timer (one runs at a time; server tracks started_at authoritatively)
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"mode":"pomodoro","task_id":"<task>"}' \
  "$CHIPFLOW_URL/api/timer/start"
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/timer/status"
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{}' "$CHIPFLOW_URL/api/timer/stop"   # completion is decided by the server
# Manual time (affects "Time spent", not pomodori)
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"task_id":"<task>","date":"2026-09-28","from":"09:00","to":"10:30","note":"..."}' \
  "$CHIPFLOW_URL/api/time/manual"
# Read or edit one entry: GET /api/time/entries/<entry> returns
# {"id","task_id","task_name","minutes","seconds","note","started_at","labels"}.
curl -s -X PUT -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"task_id":"<task>","date":"2026-09-28","from":"09:00:05","to":"09:00:29","note":"..."}' \
  "$CHIPFLOW_URL/api/time/entries/<entry>"
# from/to: HH:MM or HH:MM:SS; the entry keeps second precision. PUT replaces
# date, times, note and labels (absent note or labels clear them). A
# task_id must name an existing task (404 otherwise) and is required for an
# entry that has a task (400 without it); a task-less entry stays task-less
# when task_id is absent or "", and naming a task assigns it to that task.
# Task-less entries: sessions run without a task, breaks included (or whose
# task was deleted while they ran), are logged with task_id "" and show as
# "No task" in the timer log; GET of one returns task_name "".
# Delete a time entry: curl -s -X DELETE -H "$AUTH" "$CHIPFLOW_URL/api/time/entries/<entry>"
# Labels, due dates, comments, attachments (KanbanFlow parity)
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/boards/<board>/labels"
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"labels":["a","b"],"due_at":"2026-10-05T17:00:00Z","due_repeat":"every week","due_done":true}' \
  "$CHIPFLOW_URL/api/tasks/<task>"
# due_target_column_id: the column to reach by the due date (a column on
# the task's board; null = "Any archived column"); its name shows in
# parentheses after the due date. due_done: true marks the due date done
# (shown as "(Done)" when no target column is set). The target is cleared
# when its column is deleted or the task moves to another board.
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"body":"..."}' "$CHIPFLOW_URL/api/tasks/<task>/comments"
# author is optional (display name attribution; defaults to the
# authenticated username). "kind" is an optional tag (agent, oracle,
# review, system; max 64 chars). Comments carry server-filled actor_kind
# ("user" | "api_token") and token_name. GET .../comments returns the JSON
# comment list, oldest first.
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
# "users","tasks"}. Columns carry "former_names" (names before renames);
# "created"/"moved" history events carry "column_id" (older history has
# names only). The Export, Task count (/b/<board>/task-count),
# Throughput (/b/<board>/throughput), Burndown (/b/<board>/burndown),
# Cumulative flow (/b/<board>/cumulative-flow), Cycle & lead time
# (/b/<board>/cycle-lead), Due date performance (/b/<board>/due-date),
# Board history (/b/<board>/board-history), Calendar
# (/b/<board>/calendar), Monte Carlo forecasting
# (/b/<board>/monte-carlo), and Time estimate (/b/<board>/time-estimate)
# report pages render client-side from this.
# Calendar iCal feed (KF-351): POST /api/boards/<board>/calendar-feed ->
# {"path":"/api/calendar-feed/<token>.ics"} (bearer token created on the
# first POST; GET is read-only, {"path":null} until then); POST
# .../calendar-feed/reset rotates it;
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
# merges into the board config bag -> {"legend_visible":true,...};
# {"due_time":"08:30"} sets the time a date-only due_at gets (default 17:00;
# zero-padded HH:MM, whitespace trimmed; null leaves it unchanged).
# KF-305: {"archived":true} archives the board (hidden from default lists);
# 400 "cannot archive the last active board" when no other board is unarchived.
# KF-403: {"new_tasks_at_top":true} adds new tasks at the top of their column
# (default false: bottom).
# Task colors: POST /api/tasks {"color_id":"<color>"} assigns;
# PATCH /api/tasks/<task> {"color_id":""} clears to size-based coloring.
# Time estimates: POST/PATCH /api/tasks {"estimate_hours":4.0} sets an
# hour-based estimate (KanbanFlow parity); {"estimate_hours":0} clears it.
# Setting it derives the pomodoro size unless "size" is given explicitly.
# Task detail / subtasks / members
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/tasks/<task>"
# {"id","name","description","column_id","swimlane_id","color_id","created_at",
#  "completed_at","updated_at","member_ids","grouping_date","watched",
#  "subtasks","labels","due_at","due_repeat","due_done",
#  "due_target_column_id","metadata","links","blocked","claim","fields"}
# Claims: lease with a fencing token (one transaction; one winner).
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"claimant":"<me>","ttl_seconds":900}' "$CHIPFLOW_URL/api/tasks/<task>/claim"
# -> 200 {"claimant","role","harness","model","jj_workspace","jj_change_id",
#    "session_id","fence","started_at","heartbeat_at","expires_at"};
# 409 "task claimed by <claimant> until <expires_at>". Renew: same claimant
# plus "fence". Heartbeat: POST .../claim/heartbeat {"fence":N,
# "ttl_seconds"?,"session_id"?} (409 "claim fence mismatch or expired").
# Release: POST .../claim/release {"fence":N} (200, idempotent; 409 on a
# foreign fence). Heartbeats and same-claimant renewals leave updated_at
# alone; claim, release and expiry change it. Header "If-Claim-Fence: N" on PATCH, move and comments
# -> 409 "stale claim fence" when another fence holds the task. Expired
# claims read as null; history kinds: claimed, claim_released,
# claim_expired (logged by the first claim call that sees the expiry).
# metadata: caller-owned JSON object (null when unset). PATCH merges it by
# top-level key; a key set to null is deleted (the last one leaves null)
# and "metadata":null clears it. Not recorded in history.
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -d '{"metadata":{"agentctl":{"state":"running"}}}' "$CHIPFLOW_URL/api/tasks/<task>"
# -> 200 task detail with "metadata":{"agentctl":{"state":"running"},...}.
# 400 "metadata must be a JSON object"; 400 when the body or the merged
# object exceeds 64 KiB.
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
# completion date. Older unpadded values are padded (non-dates cleared) at
# server start.
# Custom fields (per board; values in "fields" on tasks, merged per key,
# null clears; not history):
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"key":"agentctl.checks","label":"Checks","kind":"multiline"}' \
  "$CHIPFLOW_URL/api/boards/<board>/fields"
# -> 200 {"id","key","label","kind","options","default","required","owner",
#    "position","created_at"}. kind: text|multiline|number|bool|select|
# multiselect (options for the select kinds). owner: user|system. key must
# match ^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$ (max 64; else 400 "invalid
# field key"); duplicate key -> 409 "field key exists". required is a UI
# hint, never a 400. GET .../fields lists them; PATCH /api/fields/<id>
# (label, options, default, required, position); DELETE /api/fields/<id>
# removes the values too. Task values: PATCH /api/tasks/<task>
# {"fields":{"agentctl.checks":"cargo test"}}; a bad value is a 400 naming
# the key, e.g. 'field "points": expected a number'. A move to another
# board keeps only values that board defines and accepts; dropped ones are
# listed in the "moved" history event.
# Links: kind blocked_by | blocks | related. "blocks" is stored as
# blocked_by on the target. blocked is true while a blocked_by target sits
# in a column whose is_done is false.
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"kind":"blocked_by","target_id":"<blocker>"}' "$CHIPFLOW_URL/api/tasks/<task>/links"
# -> 200 {"id","kind":"blocked_by","target_id":"<blocker>","created_at",
#    "board_id"} (board_id: the other task's board; targets may be on any
#    board).
# 400 "unknown link kind" | "target is the task itself" |
# "link would create a cycle"; 404 "Task or target not found".
curl -s -H "$AUTH" "$CHIPFLOW_URL/api/tasks/<task>/links"
# Links from this task's side (the blocker sees "blocks").
curl -s -X DELETE -H "$AUTH" "$CHIPFLOW_URL/api/tasks/<task>/links/<link>"
# 200 from either side, 404 when gone. Deleting a task deletes its links.
# History on the task holding the link: link_added / link_removed, detail
# like 'Added link blocked_by "<name>" (<uuid>)'.
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
- Task writes are all-or-nothing: a request rejected with a 4xx changes
  nothing.
- Timestamps are RFC3339 strings, except Unix seconds (UTC) for the timer
  status `started_at` and API token times; durations truncate to whole
  minutes.
- The server decides whether a pomodoro or break completed: only once its
  elapsed time reaches the duration less a 2-second grace (1498 s of 1500 s
  completes), whatever `completed` says. For a stopwatch the server takes
  `completed` as sent, and the stop reply's `completed` echoes it. Only
  completed pomodori count toward the task's pomodoro total; an earlier
  stop records an interruption (pass `"reason":"<why>"` to label it).
- `POST /api/timer/stop {"started_at":<status started_at>}` stops only that
  session; if another client already stopped or replaced it, the answer is
  200 with `"already_stopped":true` instead of a 404. Pass the status
  `session_id` as `"session_id"` for an exact match: it differs even for two
  starts in the same second (in-memory; null across a server restart, where
  `started_at` matching applies).
- Sessions under 20 seconds are discarded on stop. A session logs at most
  one hour past its duration, and a stopwatch at most 12 hours, so a timer
  left running across days does not log days of time.
- `POST /api/timer/retarget {"task_id":"<uuid>"}` re-points a running timer
  without stopping it.
