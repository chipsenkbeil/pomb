# KF Defect Fix Progress

Tracks which of the 136 parity defects (KF-001 through KF-136) are fixed.
Updated by the worker and the fidelity watchdog after each fix batch.

## Status: 13 fixed, 124 open (as of 2026-09-29 08:20 CDT)

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
Pushed as origin/main a6ceb87ca1ba87382308c1eb2805be10bf99df78 (local master 6d45f00, identical tree).
- KF-137 follow-up: adversarial testing found initEntryEdit double-binding (defer + DOMContentLoaded both fire) — manual submit double-POSTed, creating duplicate entries; calendar toggle opened-then-closed. Guarded initEntryEdit the same way TimerUI.init is guarded; task datalist now refetches on every dialog open and submit waits for it. Filed as KF-137 (initBoard/initTimerLogPage/initTimerStatsPage still unguarded). Re-verified: single click opens calendar, single submit creates exactly one entry, full 21-check suite still green.
Redeployed as origin/main 690b88e286a05ddd0d408144cc357165308650ea — chipflow.service active, public /api/v1/version build_sha matches.

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
