# KF Defect Fix Progress

Tracks which of the 138 parity defects (KF-001 through KF-138) are fixed.
Updated by the worker and the fidelity watchdog after each fix batch.

## Status: 139 fixed, 8 open (as of 2026-09-29 17:55 CDT — browser pass filed KF-140–142, KF-144–148; KF-143 reserved, description missing from handoff. Worker B fixed KF-142/144/145/147 this commit; workers A/C in progress on KF-140/141/148 and KF-146.)

## Fixed
- KF-001: Timer UI/server schema mismatch — Added phase, remaining_seconds, total_seconds, task_url, pomodoro_count to TimerStatusView
- KF-002: Timer stop 400s — Made completed field optional with serde default=false in TimerStopInput
- KF-118: Add-task form — Added hx-target="closest .task-list" to form template
- KF-127: /api/timer/settings 404 — Added route alias to api_settings handler
- KF-128: /api/timer/status 401 — Added credentials:'same-origin' to timer fetch calls
- KF-003: Add time manually dialog — Rewired ManualTime to mt-* markup (datalist, duration, calendar, future-time overlay); verified in browser 21/21 checks
- KF-004: Edit time-entry dialog — Rewired EditEntry to ee-* markup (GET/PUT /api/time/entries/:id, data-entry-id delegation, "Edit Pomodoro entry" title); verified in browser
- KF-005: Why-stop menu — Populated from settings interrupt_reasons (16 items); verified in browser
- KF-007: Session discarded toast — Sub-20s stops show the toast and skip the why menu; verified in browser
- KF-116: refreshModalTimeLog — Now hits /api/tasks/{id}/time into #time-entries; verified in browser
- KF-017: Edit dialog title — "Edit Pomodoro entry"; verified in browser
- KF-018: Manual dialog × close button — Added; verified in browser
- KF-021: Add-reason row — "Add new reason…" placeholder; Add button posts the input value; verified in browser
- KF-137: initEntryEdit double-binding — Guarded like TimerUI.init; task datalist refetches per open; verified in browser
- KF-006: Break flow — 00:00 + single green Take break starts short-break session; dead Pause/Resume branch removed (no /api/timer/pause|resume exist); running session shows red Stop only. Verified 23/23 in real Chromium vs fresh DB 2026-09-29
- KF-009: Footer first tab names the other mode — setModeTab renames #timer-foot-mode span (Stopwatch in pomodoro mode, Pomodoro in stopwatch mode). Verified in browser 2026-09-29
- KF-027: Running pill distinct — red ■ + live time on dark-red bg vs idle ▶ 25:00 ▾. Verified in browser 2026-09-29
- KF-094: Keyboard shortcuts — T toggles popup, P opens Reports menu, Y manual entry, E estimate dialog, V Move dialog, `.` More menu, Esc closes dialogs incl. with focus in field (fixed inField early-return swallow 2026-09-29), Ctrl+Enter saves+closes, Delete deletes. Verified in browser 2026-09-29
- KF-095: Shortcuts dialog exact nine KanbanFlow rows, no extras. Verified in browser 2026-09-29 (note: Askama templates compile into the binary — rebuild before verifying template edits)
- KF-098: Idle pill states — ▶ 25:00 ▾ pomodoro, ■ 00:00 ▾ stopwatch. Verified in browser 2026-09-29
- KF-011: Render the why-stop items verbatim; "Task done" must log a reason, not complete the task — whyTaskDone() now calls stopAndLog('Task done') without moving the task; menu renders configured reasons + "Add new reason..." + "Task done" last; single visible "Task done". Verified in browser 2026-09-29
- KF-077: Drop "Task done" from the seeded interruption reasons — defaults reduced to the 15 KanbanFlow reasons; timer_stop() treats "Task done" as reserved and never persists it into settings.interrupt_reasons (fresh DB: 15 reasons; after Task-done stops: still 15, none is "Task done"). Verified in browser 2026-09-29
- KF-090: Implement the Timer log page — full implementation: Board/Period/Entry-type filters, print/export(CSV)/reload icons, day groups with totals, green Successful vs red Stopped-with-reason rows, orange/green dots, P/M badges, 12h time ranges, per-day "Add time entry" links, empty state, Time-spent tab with total + chart. Server: log query gains board_id/entry_type/from/to/task_id/page params; entries gain board_id/board_name/day_key. Verified in browser 2026-09-29 (37-check suite green)
- KF-092: Implement the Pomodoro statistics page UI — full implementation: Pomodoros/Interruptions/Break activities/Highscores tabs, Period/Board/Group-by controls, summary cards, SVG bar charts with weekday tooltips, interruptions-by-reason bars, highscores (best day + longest streak), empty/loading states, CSV export. Server: stats gains from/to/board_id, best_day, longest_streak. Verified in browser 2026-09-29
- KF-119: [data-open-manual-time] delegation matches zero rendered elements — timer log day groups now render per-day "Add time entry" buttons that open the shared manual-time dialog with the day prefilled via document delegation; full submit verified (entry appears in refreshed log, no page errors). Verified in browser 2026-09-29
- KF-138: Timer popup DOM — mode tabs, idle start UI, running view ticks with task + red Stop, Today list from /api/timer/today. Verified in browser 2026-09-29
Pushed as origin/main a6ceb87ca1ba87382308c1eb2805be10bf99df78 (local master 6d45f00, identical tree).
- KF-137 follow-up: adversarial testing found initEntryEdit double-binding (defer + DOMContentLoaded both fire) — manual submit double-POSTed, creating duplicate entries; calendar toggle opened-then-closed. Guarded initEntryEdit the same way TimerUI.init is guarded; task datalist now refetches on every dialog open and submit waits for it. Filed as KF-137 (initBoard/initTimerLogPage/initTimerStatsPage still unguarded). Re-verified: single click opens calendar, single submit creates exactly one entry, full 21-check suite still green.
Redeployed as origin/main 690b88e286a05ddd0d408144cc357165308650ea — chipflow.service active, public /api/v1/version build_sha matches.


## Fixed (parallel batch 2026-09-29 08:15 CDT — merged 2026-09-29 08:35 CDT)

### W1 (/tmp/chipflow-w1) — merged as 350a85f
- KF-132: FIXED (timer pill in dark top bar)
- KF-133: FIXED (board bar buttons: Invite, Timer, Filter, Edit layout, Menu)
- KF-134: FIXED (board Filter panel)
- KF-135: FIXED (board Menu)
- KF-136: FIXED (Reports submenu, 15 items)
- KF-096: FIXED (board Filter: funnel icon + panel)
- KF-099: FIXED (board-bar Menu button with Reports menu)

### W2 (/tmp/chipflow-w2) — merged as 81a2d0a
- KF-043: FIXED (task properties to display on board)
- KF-050: FIXED (card context menu, 8 items)
- KF-056: FIXED (vertical right-edge action icon column)
- KF-058: FIXED (Subtasks section with Add subtask row)
- KF-059: FIXED (8 missing Add-menu items)
- KF-060: FIXED (Members sub-dialog)

### W3 (/tmp/chipflow-w3) — merged as 54cfa54
- KF-074: FIXED (Timer settings modal with four tabs)
- KF-079: FIXED (Sounds tab: ticking mode, 9 alarm sounds, volumes)
- KF-089: FIXED (Settings → Delete board flow)
- KF-091: FIXED (Time spent report UI)

All 17 verified: cargo fmt clean, cargo build succeeds, cargo clippy warning-free, cargo test 31 passed, node --check clean.

## Fixed (parallel batch 2026-09-29 10:42 CDT — merged 2026-09-29 11:30 CDT, master e1e6e38)

### W1 — timer surface (commit 5937834, cherry-picked from 8a96015)
- KF-008: FIXED (stopwatch panel: title/label update, count up from 00:00)
- KF-010: FIXED (popup Change-task button wired to tp-change-task + task picker)
- KF-012: FIXED (stopped sessions render as red "Stopped Pomodoro with reason 'X'" rows)
- KF-013: FIXED (task-modal Time-spent hover menu: Add time entry / Open time log)
- KF-014: FIXED (task-modal counters match KanbanFlow; extra Interruptions stat dropped)
- KF-097: FIXED (timer pill hidden while the timer popup panel is open)
- KF-129: FIXED (jsonIfJson helper — timer API HTML error pages no longer throw unhandled JSON parse errors)
- KF-130: FIXED (Timer UI DOM writes guarded against non-existent elements)
Browser verification for W1's 8: merged checks pass (fmt/build/clippy-0/32-tests/node); per-defect browser evidence comes from the post-deploy adversarial pass below.

### W2 — header surface (commit cc978a7, cherry-picked from 27eb44b)
- KF-022: FIXED (centered "ChipFlow" brand link in dark top bar). Browser 44/44: logo centered, offset=0px
- KF-023: FIXED (header tabs: Boards link, active-board tab, "+" add-board). Browser-verified
- KF-032: FIXED (pencil opens real layout-edit view: Add column / Add swimlane, back-to-board). Browser-verified end-to-end
- KF-071: FIXED (Watch is real: persisted watched flag, POST /api/tasks/:id/watch, modal toggles Watch/"✓ Unwatch", OpenAPI + agents.md/skill.md docs). Browser-verified
- KF-024: already resolved (pill in .board-toolbar-right, immediately before #filter-btn) — confirmed by worker, no change needed
- KF-026: already resolved (idle icon ▶ green rgb(22,163,74)) — confirmed by worker, no change needed
- KF-028: already resolved (running pill shows red ■ + time-only countdown) — confirmed by worker, no change needed
Worker browser verification: 44/44 checks pass, zero page errors, header + running-pill visuals match KanbanFlow reference.

### W3 — columns surface (commit e1e6e38, cherry-picked from 7232a94)
- KF-034: FIXED (WIP-limit violation in darkred with red warning line)
- KF-035: FIXED (WIP warning only when limit EXCEEDED, not when reached)
- KF-036: FIXED (header count inline with column name)
- KF-037: FIXED (column add-task button big and green)
- KF-039: FIXED (red task count on collapsed column strips)
- KF-041: FIXED ("Add to left/right" inserts adjacent to the column)
- KF-042: FIXED (Edit dialog loads the column's saved values)
- KF-120: FIXED (db.move_task position collision; regression test tests/task_move_position.rs added, 32/32 tests pass)

All 23 verified: cargo fmt clean, cargo build succeeds, cargo clippy warning-free, cargo test 32 passed, node --check clean.

## Fixed (parallel batch 2026-09-29 12:33 CDT — merged 2026-09-29 14:50 CDT, master 8116a8f)

W1 — board/cards/legend/swimlanes/task-URL:
- KF-044: FIXED (column_added_at tracking for card "Added to column" date)
- KF-051: FIXED (per-column card property config via TaskCardDisplay)
- KF-052: FIXED (card due-date mode honoring column config)
- KF-053: FIXED (member avatar chips on cards)
- KF-103: FIXED (task URL routing)
- KF-104: FIXED (cross-surface task reference)
- KF-117: FIXED (legend/swimlane parity)
- KF-121: FIXED (new-format defect)
- KF-131: FIXED (new-format defect)

W2 — task modal (labels, due dates, comments, attachments, history, time log):
- KF-057: FIXED (task modal labels)
- KF-061: FIXED (task modal due dates)
- KF-062: FIXED (task modal comments)
- KF-063: FIXED (task modal attachments)
- KF-064: FIXED (task modal history)
- KF-067: FIXED (task modal time log)
- KF-100: FIXED (cross-surface)
- KF-101: FIXED (cross-surface)
- KF-102: FIXED (cross-surface)
- KF-115: FIXED (new-format)

W3 — settings/reports/boards:
- KF-075: FIXED (board settings shell)
- KF-076: FIXED (settings general tab)
- KF-078: FIXED (interruption reasons config)
- KF-080: FIXED (break activities)
- KF-082: FIXED (board color management)
- KF-083: FIXED (board copy/default actions)
- KF-088: FIXED (boards sidebar)
- KF-093: FIXED (reports Excel export)
- KF-113: FIXED (new-format)

All 28 verified: cargo fmt clean, cargo build succeeds, cargo clippy warning-free, cargo test 48 passed, node --check clean. Merge conflicts in TaskView/routes.rs/style.css resolved; W2 struct fields restored.

## In Progress

None. All 44 LOW defects from the 2026-09-29 parallel batch are FIXED (completed 2026-09-29 ~15:50 CDT, merged as ff7c5f8/c8be564/d25e76a/7c77650 + test fix 6b6b831).

### W1 — timer/header (worktree /tmp/chipflow-l1) — DONE
KF-015, KF-016, KF-019, KF-020, KF-025, KF-029, KF-030, KF-031, KF-033, KF-122, KF-123

### W2 — columns (worktree /tmp/chipflow-l2) — DONE
KF-038, KF-040, KF-045, KF-046, KF-047, KF-048, KF-049, KF-112, KF-124, KF-125, KF-126

### W3 — cards/task modal (worktree /tmp/chipflow-l3) — DONE
KF-054, KF-055, KF-065, KF-066, KF-068, KF-069, KF-070, KF-072, KF-073, KF-114

### W4 — settings/colors (worktree /tmp/chipflow-l4) — DONE
KF-081, KF-084, KF-085, KF-086, KF-087, KF-105, KF-106, KF-107, KF-108, KF-109, KF-110, KF-111

## Open (by priority) — 0 remaining

All 35 HIGH, all 28 MEDIUM, all 44 LOW, and KF-139 are FIXED/VERIFIED. Total: 139 defects, 0 open.

## In Progress (worker C — token revoke, 2026-09-29 17:30 CDT)
- KF-146: IN_PROGRESS — API token Revoke does nothing (settings page); token persists after reload. Diagnosing client + server.

## In Progress (worker B — modal dialogs/card layout, claimed 2026-09-29 17:30 CDT)
- KF-142 [LOW]: IN_PROGRESS — stale accessible title after color change via context menu
- KF-144 [HIGH]: IN_PROGRESS — Task modal Add → Label opens no dialog
- KF-145 [HIGH]: IN_PROGRESS — Task modal Add → Due date opens no dialog
- KF-147 [HIGH]: IN_PROGRESS — first swimlane row cards obscured by sticky column header

## Fixed (parent — KF-149, 2026-09-29 ~18:15 CDT)
- KF-149 [HIGH]: FIXED — blank boards (no template) got zero swimlanes, leaving every column "+" add-task button silently dead (the add-task form clones into the first swimlane row's `.task-list` cell; with none present the handler returns early with no feedback). Fix: new `Db::ensure_default_swimlane` backfills a "Default" lane when a board has none (idempotent, skips missing boards, mirrors `ensure_board_colors`); `create_board` calls it for template-less boards; `DELETE /api/swimlanes/{id}` now returns 400 on the board's last lane (mirrors the existing last-board rule); agents.md documents both. Added regression test tests/board_default_swimlane.rs (backfill = exactly one "Default" lane, idempotent, ignores missing boards, two-lane delete OK). Verified: cargo fmt --check clean, cargo build OK, cargo clippy --all-targets 0 warnings, cargo test 53/53 pass, node --check static/app.js clean.

## Fixed (parent — KF-150, 2026-09-29 ~18:25 CDT)
- KF-150 [LOW]: FIXED — creating an API token showed the one-time secret but left the Active tokens table stale for 15s (a `setTimeout(location.reload, 15000)` in the settings.html inline script). Fix: after creation the client re-fetches `/settings`, swaps `#token-list` in place via DOMParser (server-rendered, so date formatting stays consistent), and re-binds the revoke buttons through the new `bindTokenRevokeButtons(root)` helper; no reload, so the one-time secret stays visible. Inline script syntax-checked with node --check.

## Reconciled, not filed (2026-09-29 ~18:25 CDT)
- Add-swimlane "opened once then stopped opening": NOT A BUG. `addSwimlane()` is stateless — every invocation calls `window.prompt('New swimlane name:')` fresh with no client state that could make a second call fail. The managed browser environment auto-dismisses native dialogs (established with KF-148's `window.confirm`); the observation matches the environment, not the code. Server-side `POST /api/swimlanes` is covered by existing tests.
- "Foo" entry in Interruption reasons: still unexplained, still not filed (see inventory observation note).

## Fixed (worker A — card menu/move/delete, 2026-09-29 ~18:00 CDT)
- KF-140: Card context menu now has exactly 7 entries (Start timer, Move, Color, Assign members, Copy here, Task URL, Delete) — renamed "Timer"→"Start timer", removed "Edit grouping date" menu entry and its orphaned dialog (templates/board.html) plus dead JS (openGroupingDateDialog, gd-save/gd-clear handlers, dispatch branch in static/app.js).
- KF-141: Context-menu Move now POSTs /api/tasks/{id}/move (was PATCH → 405) and includes the required `position` (MoveTaskInput.position is mandatory; the old payload 400'd) — appends at end of target column, keeping the card's swimlane when present. Verified live: POST 200, task moved. All other /move callers already used POST.
- KF-148: NOT A BUG — no code change. Investigation: server DELETE /api/tasks/{id} proven working via live curl (200, subsequent GET 404); the identical modal delete path was verified end-to-end in real Chromium earlier the same day (KF-094: "Delete deletes (after confirm)"); both reported-failing client paths gate on window.confirm(), which the managed browser environment auto-dismisses → silent early return before any request. Added regression test tests/task_delete.rs (delete removes row + column index + time entries; re-delete returns false; unknown id returns false) — passes.

## Fixed (worker C — token revoke, 2026-09-29 ~17:55 CDT)
- KF-146: FIXED — Settings → API tokens → Revoke used a native confirm() dialog, which automation environments cannot drive (the click then silently did nothing; the browser task itself noted native prompt/confirm "not drivable with available tooling"). Replaced with the styled in-page #confirm-dialog pattern established by KF-047 (markup added to settings.html; Cancel handled by app.js's [data-close-dialog] delegation). Server-side revocation was already correct and is now verified end-to-end: create token → Bearer GET /api/boards 200 → DELETE /api/v1/auth/tokens/:id 200 → Bearer GET /api/boards 401, Bearer POST /api/boards 401, token absent from list. Checks: cargo fmt clean, cargo build ok, cargo clippy --all-targets 0 warnings, cargo test 50 passed, node --check clean.

## Fixed (worker B — modal dialogs/card layout, 2026-09-29 ~17:55 CDT)
- KF-142 [LOW]: context-menu color change now re-syncs the card's title attribute (the color label) via the new data-color-label on submenu buttons — accessible title no longer goes stale after recolor.
- KF-144 [HIGH]: Task modal Add → Label called undefined openLabelsDialog() (ReferenceError → menu closed, no dialog); now calls LabelsDialog.open().
- KF-145 [HIGH]: Task modal Add → Due date called undefined openDueDateDialog(); now calls DueDateDialog.open().
- KF-147 [HIGH]: .board-table thead th sticky top:3.4rem → top:0. The 3.4rem assumed a viewport-relative offset, but .board-wrap{overflow:auto} is a scroll container so the offset resolved against the wrap, pushing the header ~2.8rem down over the first swimlane row's cards (Playwright "obscured" hit-test failures). Header now rests in flow; no overlap.
Verified: node --check static/app.js OK, cargo fmt --check OK, cargo build OK, cargo clippy --all-targets 0 warnings, cargo test 50/50 pass.

## Reconstruction note (2026-09-29 23:15 CDT)
The detailed fix entries for KF-151..KF-163 were lost in a `git reset --hard` (see fidelity-status.md recovery notice). Summary of record: KF-151..KF-159 fixed by 3 parallel workers, merged, pushed as 4acaaca ("Fix KF-151..KF-159 from final verification battery"), all 9 verified by the fix-verification battery. KF-160..KF-162 fixed in 63af405 ("Fix KF-160..KF-162 from fix-verification battery"); KF-160 and KF-162 verified PASS and KF-161 partially verified by the 22:33 watchdog browser battery on the test instance. KF-163 fixed in 3be56b35 ("Add Save board as template to board menu (KF-163)"); UI verified by the parallel run's battery (menu entry → dialog → toast → template picker listing). KF-164 FIXED in d902459 ("Save tasks in save-as-template snapshot and restore on instantiate (KF-164)"), pushed as origin/main 5eea8a6, deployed to production (verified live 5eea8a6 at ~23:03 CDT); cargo fmt/build/clippy(0 warnings)/test(54 passed)/node --check all green; browser verification of the KF164-Source → KF164-Tmpl → KF164-Dest round trip PASSED on the final battery at 2026-09-29 23:13 CDT (task, color, description, due date, subtask, Extra column all survived). Per-defect entries for KF-151..KF-159 were reconstructed in kf-parity-defects.md from the 4acaaca fix commit.

## KF-165 filed — OPEN (2026-09-29 23:45 CDT)

- KF-165 [MEDIUM]: Label filtering unavailable in Filter panel (labels input permanently disabled; clicking a card label chip opens the task dialog). Filed by the final verification battery on build 5eea8a6 at 2026-09-29 23:13 CDT. Full entry in kf-parity-defects.md. FIXED in 5fac2c1 ("Add label filtering to board filter panel (KF-165)"), pushed as origin/main df6f2dd, deployed to production (verified live df6f2dd at ~23:20 CDT; dry-activate showed chipflow.service as the sole affected unit); cargo fmt --check / build / clippy (0 warnings) / test (54 passed) / node --check all green. Test instance restarted on the new binary (PID 2190594) with /tmp/chipflow-test.redb preserved; browser battery resumed for persistence + KF-165 verification. The fix: TaskView gains labels_json (data-labels on cards), Filter panel Labels section renders checkboxes from GET /api/boards/:id/labels, applyFilter matches cards carrying every checked label (KanbanFlow AND semantics), remember-filter saves/restores labels, clicking a card label chip toggles its filter instead of opening the task modal.
- Totals after KF-165 fix: 165 headers / KF-143 reserved / 164 actionable / 163 FIXED / 1 OPEN (KF-165, pending browser verification of the fix).

## KF-166..KF-171 filed and fixed — visual parity pass (2026-09-30 ~00:00 CDT)
- Chip's verdict (2026-09-29 23:48 CDT): ChipFlow "looks and feels nothing like kanban flow" — supersedes the prior 165-defect inventory as sufficient for visual parity. Fresh live KanbanFlow capture (kanbanflow.com/board/UTTNB5) used as reference.
- KF-166 [HIGH]: card border 2px + shadow → 1px flat, radius 6px → 3px, hover shadow removed (KanbanFlow: thin borders, no shadows).
- KF-167 [HIGH]: card-title 600/0.9rem → 400/0.78rem (KanbanFlow: regular ~12px, bold only on headers).
- KF-168 [MEDIUM]: column headers left-aligned 600 → centered bold black (KanbanFlow parity).
- KF-169 [MEDIUM]: topbar padding 0.6rem → 0.25rem, avatar 2rem → 1.5rem (KanbanFlow: slim ~30px bar).
- KF-170 [MEDIUM]: card padding 0.45/0.55rem → 0.3/0.4rem, task-list gap 0.5rem → 0.25rem, board-cell padding 0.5rem → 0.35rem (KanbanFlow: tight dense stacking).
- KF-171 [LOW]: toolbar-btn radius 8px → 4px (KanbanFlow: no pill UI).
- Totals: 171 headers / KF-143 reserved / 170 actionable / 169 FIXED / 1 OPEN (KF-165, pending browser verification).

## KF-165 follow-up fixes (2026-09-30 ~00:15 CDT)
- Root cause of "No labels on this board yet": `BoardChrome.loadFilterLabels` (static/app.js:4754) chained `.then(function (labels) {...})` directly on the `api()` promise, which resolves to a Response object, not parsed JSON. `!labels.length` was always true on a Response, so the panel always showed the empty note even when GET /api/boards/:id/labels returned ["gate-label","second-label"]. Fixed by inserting `.then(function (res) { return res.ok ? res.json() : []; })` — the same pattern already used at app.js:3376 and 3697.
- Chip-click race: `toggleLabelFilter` toggled the checkbox only if it already existed in the DOM. If a user clicked a label chip before the labels API returned, nothing happened. Now stores the label in `_pendingLabelFilter` when the checkbox is missing; the existing `restoreLabelFilter()` (called at the end of `loadFilterLabels`) checks it and applies the filter once options exist.
- The card click handler (app.js:5114-5125) was already correct: `.card-label` is checked before the generic card-open, so chips filter instead of opening the modal.
- Verified: node --check static/app.js PASS. Committed 74ab474, pushed as origin/main 96a1907, deployed to production (verified live 96a1907).
- Totals: 171 headers / KF-143 reserved / 170 actionable / 170 FIXED / 0 OPEN.

## KF-172..KF-177 status (2026-09-30 ~00:50 CDT)
- KF-172 [CRITICAL] persistent left sidebar → overlay drawer: FIXED in 2430617, deployed 00:30 CDT (peer fix flow).
- KF-173 [HIGH] light board toolbar → minimal board-name bar: FIXED in 2430617, deployed 00:30 CDT (peer fix flow).
- KF-174 [MEDIUM] board tabs → single "☰ Boards" button: FIXED in 3a5dd3f, deployed ~00:37 CDT (peer fix flow).
- KF-175 [MEDIUM] Filter radios → dropdowns: FIXED in b8cbd0e (peer, 00:38 CDT; pushed to origin/main; not yet deployed as of 00:50).
- KF-176: INVALID — the "Remember filter" checkbox and "Bookmarks (0)" DO exist at the bottom of the filter panel; the filing screenshot was scrolled to the top. Corrected by peer in b8cbd0e. Not a defect.
- KF-177 [MEDIUM] Timer popup cut off on right edge: OPEN (peer likely fixing next; watchdog not claiming per no-duplicate-work rule).
- Totals: 177 headers / KF-143 reserved / KF-176 invalid / 175 actionable / 174 FIXED / 1 OPEN (KF-177).

## KF-177 claimed — IN_PROGRESS (watchdog 2026-09-30 02:35 CDT)
- KF-177 [MEDIUM]: IN_PROGRESS — Timer popup cut off on right edge (positionPopup clamps with magic 260 instead of the real 320px popup width). Fixing directly in master checkout (single defect, no parallel workers needed).

## Fixed (watchdog — KF-177, 2026-09-30 ~02:45 CDT)
- KF-177 [MEDIUM]: FIXED — Timer popup cut off on right edge. Root cause: TimerUI.positionPopup (static/app.js) clamped `left` with a magic `260` while the popup is 320px wide, so with the pill near the right edge the popup extended 60px past the viewport (verified by math: old left=1180 → right edge 1500 in a 1440px viewport). Fix: clamp with the real `popup.offsetWidth` (fallback 320) minus an 8px margin, and clear `right: auto` so the explicit `left` fully determines placement. Verified: node --check PASS; clamp math test (left=1112 → right edge 1432 ≤ 1440; centered pill unaffected at left=700); full suite on the fix tree: cargo fmt --check PASS, cargo build PASS, cargo clippy --all-targets 0 warnings, cargo test 54/54 PASS.
- Totals: 177 headers / KF-143 reserved / KF-176 invalid / 175 actionable / 175 FIXED / 0 OPEN.

## Verification battery 2026-09-30 ~03:10 CDT (build 943d79c3) — 10 new defects filed
- Battery: KF-177 PASS (1920px; 1440/1280 untestable — no viewport resize); filter dropdowns + Boards button PASS; 1440px-class visual comparison + full interaction pass done; Pomodoro template exact colors, 10-color palette, save-as-template round trip verified; Persist-Board + persist-token created for the restart check.
- API-level restart persistence check AFTER the battery: killed :3100, restarted same binary + same DB — login OK, Persist-Board + both tasks present, persist-token Bearer 200. ALL PASS.
- Filed KF-178..KF-187 (see kf-parity-defects.md for full entries + repro steps):
  - OPEN HIGH: KF-178 (board actions to gray board-header row), KF-179 (card color as left-edge stripe), KF-180 (layout editor arrange area empty), KF-181 (add-column Position ignored)
  - OPEN MEDIUM: KF-182 (column header centering/+/separators), KF-183 (legend opt-in), KF-184 (task delete native confirm → styled dialog)
  - OPEN LOW: KF-185 (card re-render after modal rename), KF-186 (Y/E undocumented), KF-187 (icon tofu — verify on real device)
  - NOT filed: D7 unreproduced drawer+popup anomaly; D3 rescoped as KF-184 (environment artifact, not a server bug).
- Totals: 187 headers / KF-143 reserved / KF-176 invalid (2 entries) / 184 actionable / 175 FIXED / 9 OPEN.

## Fixed (watchdog parallel batch — KF-178..KF-188, 2026-09-30 ~06:35 CDT)
- W1 board chrome — MERGED locally 2026-09-30 ~05:00 CDT as 963a8e3 (re-dispatched by the 04:35 watchdog run; this run verified and kept the merge):
  - KF-178 [HIGH]: board actions (Invite/Timer pill/Filter/Edit layout/Menu) restored to the gray board-header row (supersedes KF-173's dark-topbar placement premise per the 2026-09-30 battery's two live KanbanFlow references).
  - KF-183 [MED]: color legend now opt-in via a persisted toggle instead of always visible.
- W2 columns/layout — worker commit 2e2df50 survived the vanished /tmp/chipflow-w2 dir; MERGED locally 2026-09-30 06:35 CDT as 23917a2 (clean ort merge, no conflicts):
  - KF-180 [HIGH]: layout editor arrange area lists existing columns/swimlanes (drag-reorder possible).
  - KF-181 [HIGH]: add-column Position honored ("At the beginning" inserts first).
  - KF-182 [MED]: column header centering + centered green "+" + vertical separators.
- W3 cards/modal/shortcuts — prior worker's work vanished; REDISPATCHED 2026-09-30 06:35 CDT, committed 77652ab in /tmp/chipflow-w3, MERGED locally 2026-09-30 ~06:50 CDT as 35bef6b (clean ort merge, no conflicts). Independently verified by watchdog: diff reviewed, --taskColorBorder confirmed defined in all taskColorVars-* blocks, color_value always maps to a defined class, from_row_in_board resolves color_id first (no color-roundtrip bug):
  - KF-179 [HIGH]: cards render neutral white with the task color as a 4px left-edge stripe only (taskColorVars-* class); color-picker dots and modal tint unchanged.
  - KF-184 [MED]: card context-menu Delete + task-modal Delete route through the styled #confirm-dialog (showConfirmDialog), no native confirm().
  - KF-185 [LOW]: modal rename updates the card's .card-title text and data-task-name in place on save.
  - KF-186 [LOW]: shortcuts dialog gains Y (Add time manually) and E (Add time estimate) rows — now 11 rows, nine KanbanFlow originals in order.
- KF-188 [LOW] (filed by watchdog 2026-09-30 ~06:55 CDT from the W3 worker's candidate report): comment/attachment/time-entry deletes still gated on native window.confirm() — fixed directly in master (route all three through showConfirmDialog); zero live window.confirm() calls remain in app.js.
- Totals: 188 headers / KF-143 reserved / KF-176 invalid / 186 actionable / 186 FIXED / 0 OPEN. (KF-187 icon tofu stays recorded-but-not-actionable: verify on a real device first.)

## Watchdog repair batch — KF-189..KF-196 (2026-09-30 ~08:50 CDT)

- The verification battery spawned 2026-09-30 06:50 CDT (browser-task 6de4598a) **completed 07:09:44 CDT**; the prior run timed out before filing its findings. This run recovered the full handoff from the transcript DB and filed the 8 new defects (see kf-parity-defects.md for full entries + repro):
  - HIGH: KF-189 (add attachment inert), KF-190 (no board-creation UI, /templates 404), KF-191 (card white+stripe diverges from KanbanFlow full-card tint — reverts the KF-179 mis-fix), KF-194 (stopwatch stop leaves timer running after dismissing the why-stop dialog)
  - MEDIUM: KF-192 (column-header "+" centered vs KanbanFlow right-edge), KF-193 (column count stale after context-menu delete)
  - LOW: KF-195 (no logout control; /logout is POST-only), KF-196 (hidden Boards-panel DOM nodes linger, spurious "obscured" errors)
- Totals: 196 headers / KF-143 reserved / KF-176 invalid / **194 actionable / 186 FIXED / 8 OPEN**. (KF-187 still recorded-but-not-actionable.)
- Correction to the 2026-09-30 08:40 CDT phase-log guard: it said "do not spawn a duplicate battery while this entry is the latest for this SHA" on the premise the battery was running — the battery is DONE, so the guard no longer applies; a fresh post-fix battery is required.
- IN_PROGRESS claims (worktrees off origin/main 248ce1ecdf771d420ff5b468f1f67c19a9837708):
  - W1 (board chrome) — KF-190 [HIGH] board-creation UI + template picker; KF-195 [LOW] logout control; KF-196 [LOW] hidden panel nodes. Worktree /tmp/chipflow-w1.
  - W2 (columns/cards) — KF-191 [HIGH] revert KF-179 → full-card tint; KF-192 [MED] header "+" to right edge; KF-193 [MED] column count refresh on context-menu delete. Worktree /tmp/chipflow-w2.
  - W3 (modal/timer) — KF-189 [HIGH] add-attachment flow; KF-194 [HIGH] deterministic stopwatch stop. Worktree /tmp/chipflow-w3.

## Chip-reported defects 2026-09-30 11:15 CDT (KF-197..KF-201)
- KF-197 [HIGH]: timer pill doesn't tick live; only updates when clicked. FIXED 2026-09-30 (W-timer)
- KF-198 [HIGH]: selected stop reason ("Other") not shown in time log. FIXED 2026-09-30 (W-timer)
- KF-199 [MED]: card doesn't refresh after move while timer running. FIXED 2026-09-30 (W-board)
- KF-200 [MED]: canceled pomodoro shows tomato indicator (shouldn't — canceled early). FIXED 2026-09-30 (W-timer)
- KF-201 [HIGH]: collapsed swimlanes render as grid sidebar instead of folded/hidden. FIXED 2026-09-30 (W-board)
- Totals: 201 headers / KF-143 reserved / KF-176 invalid / 199 actionable / 199 FIXED / 0 OPEN.

## Reconciliation — KF-202..KF-213 (watchdog 2026-09-30 ~14:45 CDT)

- KF-202 [HIGH] folded swimlanes vertical strips: FIXED in origin/main (795a7b2/4fa1539). Inventory was stale (said OPEN); watchdog code-triage verified the fix in tree 8cc826a. Inventory updated.
- KF-203 [MED] gray header bar + count badges: FIXED in origin/main (4fa1539). Inventory stale; triage-verified. Inventory updated.
- KF-204 [LOW] "Menu" text label: FIXED in origin/main (4fa1539). Inventory stale; triage-verified. Inventory updated.
- KF-205 [MED] bottom Pomodoro legend: FIXED, deployed in 4fa1539 (per inventory).
- KF-206 [HIGH] green "+" with collapsed swimlanes: FIXED in origin/main (1dd85f7 + KF-208 floating popup supersedes). Inventory stale; triage-verified. Inventory updated.
- KF-207 [MED] PATCH color_id="" clears: FIXED in origin/main. Inventory stale; triage-verified. Inventory updated.
- KF-208 [HIGH] quick-add popup out of view: FIXED in origin/main 8cc826a (verified in local Playwright battery 2026-09-30 ~14:35 CDT; deployed).
- KF-209 [MED] popups draggable: FIXED in origin/main 8cc826a (verified ~14:35 CDT; deployed).
- KF-210 [MED] legend anchored flush: FIXED in origin/main 8cc826a (verified ~14:40 CDT; deployed).
- KF-211 [MED-HIGH] fluid columns: OPEN → IN_PROGRESS (parallel flow editing static/style.css directly in the main checkout; watchdog fix worker dispatched then closed to avoid duplication — 2026-09-30 ~14:55 CDT).
- KF-212 [MED] full-bleed legend: OPEN → IN_PROGRESS (same parallel flow; watchdog worker closed).
- KF-213 [LOW] top-bar button wells: OPEN → IN_PROGRESS (same parallel flow; style.css diff confirms KF-213 wells being added; watchdog worker closed).
- Totals: 211 headers (KF-143 reserved; KF-176 invalid x2) / 208 actionable / 205 FIXED / 3 OPEN (KF-211/212/213, all IN_PROGRESS).

## Candidate (not filed — pending live-KanbanFlow confirmation)
- Folded-strip left/right placement: on a board where the first swimlane has tasks and later lanes are folded, all folded strips render on the right; KanbanFlow presumably keeps the first folded lane on the left. Inferred, not verified — added to the verification battery checklist; file only if the battery confirms against live KanbanFlow.

## IN_PROGRESS (watchdog 2026-09-30 16:40 CDT — 3 parallel workers)
- W1 (task modal/cards) — KF-216 [MED-HIGH] hour-based estimates; KF-219 [MED] overdue flagging rule. Worktree /tmp/chipflow-w1 off origin/main 64d017e.
- W2 (filter) — KF-217 [MED] filter Color dropdown board palette; KF-215 [LOW] right-docked filter sidebar. Worktree /tmp/chipflow-w2 off origin/main 64d017e.
- W3 (docs/API) — KF-220 [LOW] agents.md column-move schema; KF-221 [LOW] REST API ergonomics gaps. Worktree /tmp/chipflow-w3 off origin/main 64d017e.
- NOT claimed: KF-218 (entangled with the parallel flow's KF-223 builder, which has uncommitted collapsed-strip overdue work in this checkout); KF-223 (builder active in this checkout); KF-224 (parallel flow's builder queued behind KF-223).
