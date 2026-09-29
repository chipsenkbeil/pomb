# KF Defect Fix Progress

Tracks which of the 138 parity defects (KF-001 through KF-138) are fixed.
Updated by the worker and the fidelity watchdog after each fix batch.

## Status: 66 fixed, 72 open (as of 2026-09-29 11:30 CDT)

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

## In Progress

(none — all 23 claimed defects resolved this batch)

## Open (by priority) — 72 remaining

All 35 HIGH defects are FIXED. Remaining: 28 MEDIUM + 44 LOW.

### MEDIUM missing/divergent — fix next
- KF-044 (columns); KF-051, KF-052, KF-053 (cards); KF-057, KF-061, KF-062, KF-063, KF-064, KF-067 (task modal); KF-075, KF-076, KF-078, KF-080, KF-082, KF-083, KF-088 (settings); KF-093 (reports); KF-100, KF-101, KF-102, KF-103 (cross-surface); KF-104, KF-113, KF-115, KF-117, KF-121, KF-131 (new-format)

### LOW — last
- KF-015, KF-016, KF-019, KF-020, KF-025, KF-029, KF-030, KF-031, KF-033, KF-038, KF-040, KF-045, KF-046, KF-047, KF-048, KF-049, KF-054, KF-055, KF-065, KF-066, KF-068, KF-069, KF-070, KF-072, KF-073, KF-081, KF-084, KF-085, KF-086, KF-087, KF-105, KF-106, KF-107, KF-108, KF-109, KF-110, KF-111, KF-112, KF-114, KF-122, KF-123, KF-124, KF-125, KF-126
