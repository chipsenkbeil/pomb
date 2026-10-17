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

### Device pairing (CLI login without pasting a token)

A command-line client can get its own token through the RFC 8628 device
flow. The two API calls need no credentials; the approval needs the
browser session, never a Bearer token.

```bash
curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"client_name":"agentctl on laptop","scopes":["read","write"]}' \
  "$BASE/api/v1/auth/device"
# 200 {"device_code":"<43-char secret>","user_code":"BCDF-GHJK",
#      "verification_url":"https://chipflow.example/device",
#      "interval_seconds":5,"expires_in_seconds":600}
# client_name (1-80 characters) and scopes (a non-empty subset of
# ["read","write"]) are required, else 400. 429 "too many pending device
# requests" while 20 requests await approval.
```

Show the person `user_code` and `verification_url`. Logged in to ChipFlow,
they open the URL, type the code (case, spaces and the dash do not
matter), see the client name and scopes, and approve or deny. Ten wrong
codes within ten minutes, typed on the code form or sent to approve or
deny, lock all three for the rest of that window (429), even for a right
code. Only codes that never existed, or expired, count; resubmitting a
code that was already approved or denied does not. The count is kept in
memory per server and resets on restart.

```bash
curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"device_code":"<device_code>"}' "$BASE/api/v1/auth/device/token"
# 200 {"token":"cf_...","name":"agentctl on laptop","scopes":["read","write"]}
#   once approved. The token is returned this one time; it is an ordinary
#   API token, listed and revocable under Settings -> API tokens.
# 428 "authorization_pending"  not approved yet; poll again later
# 429 "slow_down"              polled sooner than interval_seconds after the
#                              previous poll; wait longer (RFC 8628 says to
#                              add 5 seconds to the interval)
# 403 "denied"                 the person denied it
# 410 "expired"                not approved within expires_in_seconds
# 400 "invalid_grant"          unknown device_code, or already exchanged
# 400 "device_code is required"
```

The device code is a single-use secret, stored only as a hash. Expired and
spent requests are deleted lazily, a day after they expire, when the next
pairing starts.

Browser login (`POST /login`) sets the session cookie with the `Secure`
attribute only when the request arrived over HTTPS (e.g. behind a
TLS-terminating proxy). Over plain HTTP the cookie has no `Secure`
attribute, so cookie auth works on local dev servers.

Writes (any method but GET/HEAD/OPTIONS) authenticated by the browser
cookie must be same-origin: a request whose `Sec-Fetch-Site` or `Origin`
header marks it as coming from another origin gets
`403 cross-site request refused`; Bearer-token clients are unaffected.

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
- Task writes are all-or-nothing: creating, patching, moving, commenting
  on, linking, claiming and watching a task, logging time on it, and
  adding, editing or deleting its subtasks and attachments validate the
  whole request first and save it, history events included, in one
  transaction. A request rejected with a 4xx changes nothing, with one
  documented exception: a 409 heartbeat or release against an expired
  claim records that expiry (it appends a `claim_expired` event and bumps
  `updated_at`), although every view already reported the claim as null.
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
curl -s -H "$AUTH" "$BASE/api/boards/<board-uuid>/tasks"
# [{"id":"<task-uuid>","name":"Write release notes",
#   "column_id":"<column-uuid>","swimlane_id":"<swimlane-uuid>",
#   "position":0.0,"labels":["agent"],"member_ids":[],
#   "updated_at":"2026-10-16T18:02:11.482113+00:00",
#   "metadata":{"agentctl":{"state":"queued"}},"blocked":false,
#   "claim":null,"fields":{"stage":"review"}}]
# Board order: column, then swimlane, then position. Optional filters,
# all of which must match:
#   column_id=<uuid>, swimlane_id=<uuid>
#   label=<name>, repeatable: label=a&label=b keeps tasks carrying both
#   since=<RFC3339>: tasks whose updated_at is after that instant. Use a
#     Z offset or percent-encode "+" as %2B (a bare "+" decodes to a
#     space); anything unparseable is 400 'invalid since "<value>": use
#     RFC3339'.
#   ready=true: tasks that are not blocked (see Task links below);
#     ready=false: blocked tasks only. Anything but true/false is 400
#     'invalid ready "<value>": use true or false'.
#   claimed=true: tasks with a live claim (see Claims below);
#     claimed=false: unclaimed tasks. Same 400 rule as ready.
# 404 "Board not found" for an unknown board id.
curl -s -H "$AUTH" \
  "$BASE/api/boards/<board-uuid>/tasks?column_id=<column-uuid>&label=agent&since=2026-10-16T18:00:00Z"
# updated_at changes on every write to the stored task, including a
# position shift caused by a neighbor's move and each history event.
# updated_at also changes when a task's blocked value changes: a blocker
# entering or leaving a Done column, a column's is_done being toggled, or
# a blocker being deleted stamps every dependent (on any board) whose
# blocked value flipped, so since=...&ready=true finds newly ready tasks.
# Tasks stored before updated_at existed report created_at until their
# next write.
curl -s -H "$AUTH" "$BASE/api/tasks?board_id=<board-uuid>" | head -c 600
# Tasks as {"id","name","board_id"} items (autocomplete list): every board's
# tasks, or one board's with board_id= (empty or blank value: no filter; 404
# "Board not found" for an unknown board). For full task fields use /api/boards/<board-uuid>/tasks.
```

### Create a task

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"Write release notes","column_id":"<column-uuid>","size":2}' \
  "$BASE/api/tasks"
# size: 1..4 pomodori (4 = ">3"). column_id is required; swimlane_id is
# optional (defaults to the board's first swimlane).
# Default response is the rendered card HTML fragment (the UI appends it).
# Send 'Accept: application/json' for a 201 JSON body instead: the task
# detail, the same shape as GET /api/tasks/<task-uuid> below.
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -d '{"name":"Write release notes","column_id":"<column-uuid>","description":"From the changelog","labels":["docs","agent"]}' \
  "$BASE/api/tasks"
# 201 {"id":"<task-uuid>","name":"Write release notes",
#      "description":"From the changelog","column_id":"<column-uuid>",
#      "swimlane_id":"<swimlane-uuid>","created_at":"...","updated_at":"...",
#      "labels":["docs","agent"],...}
# description and labels are optional; labels are trimmed, and empty or
# duplicate labels are dropped.
# estimate_hours: optional hour-based time estimate (KanbanFlow parity),
# e.g. {"estimate_hours":4.0}; the pomodoro size is derived from it unless
# size is also given.
# due_at: optional due date/time at creation, same formats as the PATCH
# below; an unparseable value is a 400 and creates no task.
# due_repeat: optional repeat text, e.g. "every week"; kept only with a due_at.
# A task created in a Done column gets its completion time at once.
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
# column_id is on another board. A move to another board also gives the
# task that board's color of the same value, else that board's default.
# position is the index among the target cell's other tasks (rounded,
# clamped to the ends). In a Done column it counts them in on-screen order
# (date groups: Today first, then future and past days newest first,
# undated last) and the task lands beside the neighbor at that index from
# its own date group; with no
# such neighbor it keeps its place, or joins the top of its group when it
# comes from another cell. Moving into a Done column sets the task's
# completion time (the task modal shows "✓ Done (<date>)", the card shows
# no stamp); moving out clears it.
# WIP limits: moving into another column whose wip_limit it already
# reaches (counting every swimlane, not the moving task) is 409 "column
# WIP limit reached (n/limit)", e.g. (3/3); nothing changes. A move within
# the task's own column is never refused. Add ?force=true to move anyway
# (the board UI always does, so people can still drag into a full column):
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"column_id":"<column-uuid>","position":0}' \
  "$BASE/api/tasks/<task-uuid>/move?force=true"
# 400 "unknown column" for a column id that does not exist.
# Deleting a column: DELETE /api/columns/<column-uuid>. 400 while it still
# holds tasks (move them first), and 400 "cannot delete the board's last
# column" for a board's only column.
```

### Task detail, subtasks, and members

```bash
curl -s -H "$AUTH" "$BASE/api/tasks/<task-uuid>"
# {"id":"<task-uuid>","name":"Write release notes",
#  "description":"From the changelog","column_id":"<column-uuid>",
#  "swimlane_id":"<swimlane-uuid>","color_id":"<color-uuid>",
#  "created_at":"2026-10-16T18:02:11.482113+00:00","completed_at":null,
#  "updated_at":"2026-10-16T18:05:40.019876+00:00",
#  "member_ids":[],"grouping_date":null,"watched":false,
#  "subtasks":[{"id","name","done"}],"labels":[],"due_at":null,
#  "due_repeat":null,"due_done":false,"due_target_column_id":null,"metadata":null,
#  "links":[{"id","kind","target_id","created_at","board_id"}],
#  "blocked":false,"claim":null,"fields":{}}
# color_id is null for a task without a color; completed_at is the RFC3339
# time the task entered a Done column, null while it is not done.
# Comments come from GET .../comments. 404 "task not found".

curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -d '{"name":"Write the release notes"}' "$BASE/api/tasks/<task-uuid>"
# With 'Accept: application/json', PATCH returns 200 with the updated task
# detail (same shape as above); without it, the refreshed card HTML.

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
# its completion date. Both fields optional. Grouping dates stored before
# this rule are rewritten zero-padded when the server starts, or cleared
# when they are not a date; the server log names each changed task.

curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"watched":true}' "$BASE/api/tasks/<task-uuid>/watch"
# Task "Watch" toggle (KanbanFlow parity): sets the task's persisted watched
# flag, shown in the modal More menu. Returns {"watched":true}.
```

### Task metadata

`metadata` is a JSON object the caller owns; the server checks only that it
is valid JSON and an object, never its contents. It appears on the task
detail and on board task list entries (`null` when unset), and the task
dialog shows it read-only under the description.

```bash
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -d '{"metadata":{"agentctl":{"state":"running","attempt":2}}}' \
  "$BASE/api/tasks/<task-uuid>"
# 200 {"id":"<task-uuid>",...,"metadata":{"agentctl":{"state":"running","attempt":2},"owner":"chip"}}
# The merge is per top-level key: "agentctl" is replaced whole and other
# keys ("owner" here) stay as they were.
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"metadata":{"owner":null}}' "$BASE/api/tasks/<task-uuid>"
# A key set to null is deleted. Deleting the last key leaves
# "metadata": null, never {}.
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"metadata":null}' "$BASE/api/tasks/<task-uuid>"
# "metadata": null clears every key; the task reports "metadata": null.
# Metadata changes are not recorded in the task history.
# 400 "metadata must be a JSON object" for an array, string, number or
# boolean. 400 "request body exceeds 64 KiB" when a request carrying
# metadata is larger than 64 KiB, and 400 "metadata exceeds 64 KiB" when
# the merged object would be. A rejected request changes nothing.
```

### Task links and readiness

A link joins two tasks: `blocked_by`, its inverse `blocks`, or `related`.
A `blocks` link is stored as `blocked_by` on its target, so both forms of
the same dependency are one link with one id. A task is `blocked` while any
of its `blocked_by` targets sits in a column whose `is_done` is false; a
blocker in any Done column, whatever its name, no longer blocks. A link
may target a task on another board. Board cards show a link icon with the
link count (red while blocked); hover it for the linked task names, each
followed by its board's name in parentheses when it sits on another board.

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"kind":"blocked_by","target_id":"<blocker-uuid>"}' \
  "$BASE/api/tasks/<task-uuid>/links"
# 200 {"id":"<link-uuid>","kind":"blocked_by","target_id":"<blocker-uuid>",
#      "created_at":"2026-10-16T18:10:02.118251+00:00",
#      "board_id":"<blocker's-board-uuid>"}
# board_id is the other task's current board (it follows that task across
# boards), so a client can tell cross-board links apart.
# Asking again for a link that already exists (in either form, or a
# related link from the other side) returns it unchanged.
# 400 "unknown link kind" (kind not blocked_by|blocks|related),
# 400 "target is the task itself", 400 "link would create a cycle" (the
# blocked_by chain would lead back to the task), 404 "Task or target not
# found".

curl -s -H "$AUTH" "$BASE/api/tasks/<blocker-uuid>/links"
# [{"id":"<link-uuid>","kind":"blocks","target_id":"<task-uuid>",
#   "created_at":"...","board_id":"<task's-board-uuid>"}]
# Each task lists its links from its own side: the blocked task sees
# blocked_by, the blocker sees blocks, and both see related. Oldest
# first; 404 "task not found".

curl -s -X DELETE -H "$AUTH" "$BASE/api/tasks/<task-uuid>/links/<link-uuid>"
# 200 from either task's side; 404 "link not found" afterwards.
# Deleting a task (or its board) deletes every link to or from it.
# History: adding a link appends "link_added" and removing one appends
# "link_removed", both on the task that holds the link (the blocked task
# for blocked_by and blocks, the requesting task for related). The detail
# names the stored kind and the other task, e.g.
#   link_added    Added link blocked_by "Write docs" (<blocker-uuid>)
#   link_removed  Removed link blocked_by "Write docs" (<blocker-uuid>)
# A link removed because its other task was deleted logs link_removed with
# " (task deleted)" appended and actor "system". Asking for an existing
# link logs nothing.

curl -s -H "$AUTH" "$BASE/api/boards/<board-uuid>/tasks?ready=true"
# Unblocked tasks, each with "blocked":false.
```

### Custom fields

Each board can define typed custom fields; tasks carry their values in
`fields`, a `{<key>: value}` object on the task detail and on board task
list entries (`{}` when none). Fields are for people and agents alike;
`metadata` stays the machine-only store. Field changes are not recorded
in the task history.

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"key":"agentctl.checks","label":"Checks","kind":"multiline","required":true}' \
  "$BASE/api/boards/<board-uuid>/fields"
# 200 {"id":"<field-uuid>","key":"agentctl.checks","label":"Checks",
#      "kind":"multiline","options":[],"default":null,"required":true,
#      "owner":"user","position":0,"created_at":"2026-10-17T09:00:00+00:00"}
```

- `key` (required): dotted lowercase segments, each starting with a letter,
  at most 64 characters: `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`. Anything
  else is 400 "invalid field key". A key the board already has is 409
  "field key exists", so a client can create-or-adopt.
- `label` (required): 400 "label is required" when missing or blank.
- `kind` (required): `text` (one line: no `\n`, `\r`, U+0085, U+2028 or
  U+2029), `multiline` (a
  string with newlines), `number`, `bool`, `select`, or `multiselect`. 400 "kind is
  required" or "unknown field kind".
- `options`: required, non-empty and unique for `select` and
  `multiselect`, and not allowed for other kinds. An option holding a line
  break is 400 `field "<key>": options must be single lines`.
- `default`: checked like a value; new tasks start with it when they do not
  set the field. 400 `field "<key>": default: <reason>` otherwise.
- `required`: a UI hint only, never a 400. Tasks are created and patched
  without it; the task dialog marks the empty input and the card shows a
  "fields incomplete" badge while a required user field is empty.
- `owner`: `user` (default; editable in the task dialog) or `system`
  (shown read-only in the dialog's "System fields" block). 400 "unknown
  field owner" otherwise.
- 404 "Board not found".

```bash
curl -s -H "$AUTH" "$BASE/api/boards/<board-uuid>/fields"
# [FieldDef, ...] by position.
curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"label":"Stage","options":["draft","review","done"],"default":"draft","required":false,"position":2}' \
  "$BASE/api/fields/<field-uuid>"
# 200 FieldDef. Only label, options, default (null clears it), required and
# position change; key, kind and owner are fixed, and sending any of them is a
# 400 "<name> cannot be changed". A default that no longer fits the options
# is a 400. 404 "field not found".
curl -s -X DELETE -H "$AUTH" "$BASE/api/fields/<field-uuid>"
# 200; also deletes the field's value on every task of the board.

curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -d '{"fields":{"agentctl.checks":"cargo test\ncargo clippy","stage":"review","note":null}}' \
  "$BASE/api/tasks/<task-uuid>"
# 200 task detail with "fields":{"agentctl.checks":"cargo test\ncargo clippy","stage":"review"}.
# Values merge per key; null clears a key. POST /api/tasks takes "fields"
# the same way. Every 400 names the key:
#   unknown field "nope"
#   field "points": expected a number
#   field "stage": "done" is not an option
#   field "note": text must be a single line; use a multiline field
# Other value errors: expected a string, expected true or false, expected
# one option as a string, expected an array of options. Multiselect
# choices are deduplicated. "fields" must be an object; anything else,
# null included, is 400 "fields must be a JSON object" on PATCH and POST.
# Values a field's options no longer list stay stored until changed; the
# task dialog marks them "(not an option)", and a required field holding
# one counts as incomplete (the card shows "fields incomplete").
# Moving a task to another board keeps a field value only when that board
# defines the same key and accepts the value; the rest are dropped and
# written into the move's history event so a mistaken move can be undone
# by hand, e.g.
#   moved  Moved from Todo to Backlog; dropped fields: {"only_here":"x","stage":"review"}
# Moves within a board leave fields alone.
```

Board settings has a **Custom fields** page (`/b/<board-uuid>/settings/fields`)
for adding, editing and deleting definitions (options one per line).

### Claims: leases with a fencing token

A claim is a lease on one task. Each successful claim gets a `fence`, a
per-task integer that goes up by one on every new claim, so a stale
worker's writes can be refused. The check and the write happen in one
database transaction: of two simultaneous claims, exactly one gets 200.

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"claimant":"builder-i","ttl_seconds":900,"role":"builder","harness":"claude-code","model":"opus","jj_workspace":"feat-x","jj_change_id":"kxqv","session_id":"s-41"}' \
  "$BASE/api/tasks/<task-uuid>/claim"
# 200 {"claimant":"builder-i","role":"builder","harness":"claude-code",
#      "model":"opus","jj_workspace":"feat-x","jj_change_id":"kxqv",
#      "session_id":"s-41","fence":3,
#      "started_at":"2026-10-17T09:00:00.120934+00:00",
#      "heartbeat_at":"2026-10-17T09:00:00.120934+00:00",
#      "expires_at":"2026-10-17T09:15:00.120934+00:00"}
# claimant and ttl_seconds (1..86400) are required; the rest are optional.
# 409 "task claimed by builder-j until <expires_at>" while another live
# claim holds the task. The same claimant sending its live "fence" renews
# the lease (same fence and started_at, new heartbeat_at and expires_at,
# and any optional field it sends replaces the stored one); the same
# claimant without that fence gets the same 409.
# 400 "claimant is required", "ttl_seconds is required",
# "ttl_seconds must be between 1 and 86400"; 404 "task not found".

curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"fence":3,"ttl_seconds":900,"session_id":"s-42"}' \
  "$BASE/api/tasks/<task-uuid>/claim/heartbeat"
# 200 TaskClaim with new heartbeat_at and expires_at. Without ttl_seconds
# the lease keeps its previous length. 409 "claim fence mismatch or
# expired"; 400 "fence is required".

curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"fence":3}' "$BASE/api/tasks/<task-uuid>/claim/release"
# 200 (empty body); the task's "claim" is null. With no live claim, any
# fence the task has issued releases with 200, so retries are safe.
# 409 "claim fence mismatch" when a live claim holds another fence or the
# fence was never issued.

curl -s -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'If-Claim-Fence: 3' -d '{"name":"..."}' "$BASE/api/tasks/<task-uuid>"
# If-Claim-Fence (optional) on PATCH /api/tasks/<id>, POST .../move and
# POST .../comments: 409 "stale claim fence" when a live claim holds a
# different fence; 400 "invalid If-Claim-Fence: use an integer". Without a
# live claim the header passes. Humans and the UI omit it.
```

Expiry is lazy, with no background job. Every read treats a claim whose
`expires_at` is in the past as `"claim": null`, and reads never write. The
history records each lease change on the task:

- `claimed`: a new claim (renewals add nothing).
- `claim_released`: a release by the live fence.
- `claim_expired`: appended once, by the first claim, heartbeat or release
  call on the task that finds the stored claim expired. That call clears
  the stored claim, so later calls cannot log it again. A new claim after
  an expiry logs `claim_expired` and then `claimed`. A task whose expired
  claim is never touched again has no `claim_expired` event.

A claim, a release and an expiry each change the task's `updated_at`. A
heartbeat, and a renewal through `POST .../claim` by the same claimant with
its live fence, change only the lease (`heartbeat_at`, `expires_at`, and any
optional claim field sent) and leave `updated_at` alone, so `?since=`
polling does not return every claimed task on every tick.

Board cards show the live claimant with a lock icon; hover it for the
lease end.

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
# due_at: surrounding whitespace is trimmed; null, an empty string or
# whitespace-only clears the due date. Accepted forms:
# 1. RFC3339, stored as sent once trimmed. As RFC3339 allows, a lowercase
#    "t" or a space may replace the "T" and a lowercase "z" the "Z".
# 2. "YYYY-MM-DD HH:MM" or "YYYY-MM-DD HH:MM:SS" in the server's local
#    timezone, with exactly one space or uppercase T between date and time.
# 3. A bare "YYYY-MM-DD", due that day at the board's Due date time
#    (5:00 PM unless changed, see the board config below) in the server's
#    local timezone.
# The year is always four digits with no sign. In forms 2 and 3 every
# other field may drop its leading zero ("2026-1-5", "2026-10-20 8:5:7").
# Seconds run 00-59 in every form, RFC3339 included, so a ":60" leap
# second is a 400. Anything else is a 400 and the request changes
# nothing. A local time that occurs twice at a daylight-saving fall-back
# resolves to the earlier instant; a local time that never occurs because
# a spring-forward skips it is a 400. Both use the server zone's exact
# transition second: in London, "2026-10-25 01:00" to "01:59:59" occur
# twice and "02:00" occurs once (02:00Z), while "2027-03-28 01:00" to
# "01:59:59" never occur. Re-sending the same instant in another format
# (e.g. ".000Z") is not a change: the stored string and due_done stay. A
# task is flagged overdue only once the full due timestamp has passed,
# never merely on the due date.
# due_repeat: free text, e.g. "every week"; empty string or null clears it.
# A repeat needs a due date: clearing due_at clears due_repeat too, and a
# repeat sent for a task with no due date is dropped.
# due_target_column_id: KanbanFlow's "Target column", the column the task
# should reach by its due date; a column on the task's board (otherwise a
# 400). Its name follows the due date on the card and in the modal, e.g.
# "Due: Friday 5:00 PM (Done)". Null or "" means "Any archived column"
# (no parenthetical); a target sent for a task with no due date (and none in
# the same request) is a 400 "due_target_column_id needs a due date";
# clearing due_at clears it too, and so does deleting
# the target column or moving the task to another board.
# due_done: true/false marks the due date done; without a target column
# the card then shows "(Done)".

curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"body":"Checks passed","kind":"agent"}' "$BASE/api/tasks/<task-uuid>/comments"
# Returns the created comment:
# {"id":"<comment-uuid>","author":"api-token","body":"Checks passed",
#  "created_at":"2026-10-17T09:30:00+00:00","actor_kind":"api_token",
#  "token_name":"agentctl-bot","kind":"agent"}
# author is optional (a display name to attribute the comment to); it
# defaults to the authenticated username ("api-token" for API tokens).
# kind is an optional free-form tag (agentctl uses "agent", "oracle",
# "review" and "system"); blank means untagged (null), and more than 64
# characters is 400 "comment kind must be at most 64 characters". The task
# dialog shows it as a small tag beside the comment's date.
# actor_kind ("user" or "api_token") and token_name (the API token's name,
# or null for the browser session and the CHIPFLOW_API_TOKEN bootstrap
# token) are filled in by the server; comments stored before these fields
# existed read as "user" with null token_name and kind.
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
# {active, mode, mode_title, task_id, task_name, task_color, started_at, duration_secs,
#  remaining_seconds, elapsed_seconds, pomodoro_count}. pomodoro_count = today's
#  completed pomodori; elapsed_seconds keeps counting past the duration.
```

### Stop the timer

```bash
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"completed":true}' \
  "$BASE/api/timer/stop"
# The server decides completion for timed sessions: a pomodoro or break counts
# as completed (green "Successful Pomodoro") only once its elapsed time reaches
# the duration, less a 2-second grace for the client's 1 s tick (1498 s of a
# 1500 s pomodoro completes, 1497 s does not), whatever "completed" says.
# A stopwatch has no duration, so for it the server takes "completed" as sent.
# The reply {"discarded","minutes","completed","already_stopped"} echoes that
# verdict: for a stopwatch "completed" is the flag you sent (false when
# discarded or already stopped). An earlier pomodoro stop is interrupted
# (a stopwatch entry never is); add "reason":"<why>" (red "Stopped Pomodoro
# with reason "<why>""). Sessions under 20 seconds are discarded.
# A session logs at most one hour past its duration (a stopwatch at most 12
# hours), so a timer left running across days does not log days of time.
# Sessions without a task are logged too, breaks included (task_id "",
# task_name "No task"), as is one whose task was deleted while it ran; a
# task-less break with an activity_id counts toward that activity in
# /api/timer/statistics (not when filtering by board_id).
# A break session can carry "activity_id":"<break-activity-uuid>" to attribute
# its time to a defined break activity (feeds the statistics Break activities tab).
# Pass "started_at":<status started_at> to stop only that session: when it is no
# longer active (another client stopped or replaced it) the stop is a 200 with
# "already_stopped":true and changes nothing. Without started_at or session_id,
# stopping an idle timer is a 404; with either, it is a 200 already_stopped.
# Status and start also return "session_id", an in-memory id that differs even
# for two starts in the same second and never repeats across a server restart.
# Pass "session_id" to stop for an exact match: a stale id (also one from before
# a restart) gets already_stopped:true and the newer session keeps running. A
# session running across a restart reports session_id null and a stop then
# matches by started_at alone (a second-granular match). The web UI sends both.
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

### Edit a time entry

```bash
curl -s -X PUT -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"task_id":"<task-uuid>","date":"2026-09-28","from":"09:00:05","to":"09:00:29"}' \
  "$BASE/api/time/entries/<entry-uuid>"
# from/to: HH:MM or HH:MM:SS 24h; the entry keeps second precision (24s here).
# GET /api/time/entries/<entry-uuid> returns "seconds" alongside "minutes".
# task_id is required for an entry that has a task (400 without it); a
# task-less entry (task_id "") stays task-less when task_id is absent or "".
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
# Days (from/to, day_key, the TODAY list) are calendar days in the server's zone.
curl -s -H "$AUTH" "$BASE/api/timer/time-spent?from=2026-09-01&to=2026-09-30"
curl -s -H "$AUTH" "$BASE/api/timer/time-spent?from=2026-09-01&to=2026-09-30&board_id=<board-uuid>"
curl -s -H "$AUTH" "$BASE/api/timer/time-spent?from=2026-09-01&to=2026-09-30&color_id=<color-uuid>"
curl -s -H "$AUTH" "$BASE/api/timer/statistics"
curl -s -H "$AUTH" "$BASE/api/timer/statistics?from=2026-09-01&to=2026-09-30&board_id=<board-uuid>"
```
Log filters: `task_id`, `board_id`, `entry_type` (pomodoro|stopwatch|manual),
`from`/`to` (YYYY-MM-DD). Log entries carry `board_id`, `board_name`,
`day_key`, `badge_code` (P pomodoro, S stopwatch, B break, M manual), `badge_title`,
`time_range`, `task_color` (the task colour's border shade, null without one). Statistics accept
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
# PATCH {"config_json":"{...}"} stores the Edit column settings (a JSON
# string). Keys with an effect: "sorting" ("none", "due", "color",
# "name", "created", "added") orders the column's tasks; "column_sum"
# ("none", "count", "hours") picks the header badge: none, the task count
# or the estimated hours (columns with a WIP limit show "count / limit"
# instead); "group_by_date" (true/false, Archiving) groups the column's
# tasks under date headers, shows the newest 20 with "Load more tasks"
# for the rest, and defaults to true for the Done column.
# GET /api/boards/<board-uuid>/columns lists the board's columns as
# [{"id","name","wip_limit","is_done"}] (wip_limit is null when unset).

curl -s -H "$AUTH" "$BASE/api/boards/<board-uuid>/export-data"
# The board reports' shared dataset: {"board":{"id","name"},
# "columns":[{"id","name","former_names"}], "swimlanes":[{"id","name"}],
# "colors":[{"id","value","label"}], "users":[{"id","username"}],
# "tasks":[full task rows]}. export-data is the raw stored state, without
# lazy expiry: an expired claim still appears there. former_names lists a
# column's names before
# its renames, oldest first. Each task's "history" holds its events; a
# "created" or "moved" event also carries "column_id", the column it put
# the task in, which stays set after that column is deleted. History
# recorded before column ids existed has names only, so a rename made
# before then can still show as a separate band in the Cumulative flow
# chart. The Export report page (/b/<board-uuid>/export)
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
# Calendar iCal feed (KF-351): POST /api/boards/<board-uuid>/calendar-feed
# returns {"path":"/api/calendar-feed/<token>.ics"} — the token is a 32-byte
# bearer secret created on the first POST, revealed only by these endpoints
# (never in page source). GET on the same path is read-only and returns
# {"path":null} until a token exists.
# POST /api/boards/<board-uuid>/calendar-feed/reset rotates it.
# GET /api/calendar-feed/<token>.ics is unauthenticated by design (calendar
# clients can't do cookie auth) and serves text/calendar with one VEVENT per
# task due date (minimal fields; RRULE for recognized due_repeat cadences).
```

### Boards and board templates

```bash
curl -s -H "$AUTH" "$BASE/api/boards"
curl -s -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"Side project","template_id":"<template-uuid>"}' "$BASE/api/boards"
# name: 1-100 characters after trimming, else 400.
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
colors. Refuses (400) when it is the last remaining board, or the last
unarchived one while archived boards remain (unarchive one first).

```bash
curl -s -H "$AUTH" -X PUT -H 'Content-Type: application/json' \
  -d '{"legend_visible":true}' "$BASE/api/boards/<board-uuid>/config"
```

Merges the given fields into the board's opaque config bag (e.g. the Color
legend Menu toggle persists `legend_visible` here, per board); responds with
the current config, e.g.
`{"legend_visible":true,"archived":false,"due_time":"17:00"}`.

KF-404: `{"due_time":"08:30"}` sets the board's Due date time, the time a
date-only `due_at` gets (Board settings > Task settings shows it). It takes
zero-padded 24-hour "HH:MM" (surrounding whitespace is trimmed); anything
else is a 400. Null or absent leaves it unchanged. Boards that never set
it use 17:00.

KF-305: `{"archived":true}` archives the board (hidden from `GET /api/boards`
and the Boards dropdown unless `?include_archived=true`); `{"archived":false}`
unarchives. Archived boards are listed at `/boards/archived`. Archiving the
only unarchived board is a 400 ("cannot archive the last active board").

KF-403: `{"new_tasks_at_top":true}` makes `POST /api/tasks` put new tasks at
the top of their column; `false` (the default) puts them at the bottom.

Board footer semantics (KF-222): the color-legend footer renders only when
`legend_visible` is true — no footer markup at all when false. When on, it
shows one labeled segment per enabled board color in palette order, labeled
with each color's custom name. New boards default to false (KanbanFlow
parity); boards predating the toggle were backfilled to true.

## Notes for agents

- Prefer `GET /api/v1/openapi.json` over guessing at undocumented paths.
- The timer keeps server-side state: `started_at` is authoritative. The web
  UI counts overtime past the duration; stop whenever, and the logged
  minutes reflect actual elapsed time, up to one hour past the duration
  (12 hours for a stopwatch). The board card's live "+ time" shows that
  same capped amount, while the header pill keeps counting the full elapsed
  time.
- Only a pomodoro that reached its duration (by the server's clock, with a
  2-second grace) counts toward the task's pomodoro total. An earlier stop
  logs its time and records an interruption instead, even with
  `completed:true`.
- `POST /api/timer/retarget {"task_id":"<uuid>"}` re-points a running timer
  at a different task without stopping it.
- Keep the token out of logs, files, and chat transcripts. If a token leaks,
  revoke it in Settings → API tokens and mint a new one.
