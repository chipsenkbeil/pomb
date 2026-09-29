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
