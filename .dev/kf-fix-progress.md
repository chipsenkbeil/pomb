# KF Defect Fix Progress

Tracks which of the 138 parity defects (KF-001 through KF-138) are fixed.
Updated by the worker and the fidelity watchdog after each fix batch.

## Status: 43 fixed, 95 open (as of 2026-09-29 08:35 CDT)

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

## Open (by priority)
### HIGH broken (functional bugs) — fix first
- KF-003: Add time manually dialog

### HIGH missing/divergent
- KF-132: Timer in board bar (not top header)
- KF-133: Board bar buttons
- KF-134: Filter panel
- KF-135: Board Menu
- KF-136: Reports submenu
- (plus other HIGHs from KF-004 through KF-126 — see defect file)

### MEDIUM and LOW
- See kf-parity-defects.md for full list KF-004 through KF-126, KF-129 through KF-131
