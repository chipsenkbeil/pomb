# KF Defect Fix Progress

Tracks which of the 136 parity defects (KF-001 through KF-136) are fixed.
Updated by the worker and the fidelity watchdog after each fix batch.

## Status: 5 fixed, 131 open (as of 2026-09-29 00:30 CDT)

## Fixed
- KF-001: Timer UI/server schema mismatch — Added phase, remaining_seconds, total_seconds, task_url, pomodoro_count to TimerStatusView
- KF-002: Timer stop 400s — Made completed field optional with serde default=false in TimerStopInput
- KF-118: Add-task form — Added hx-target="closest .task-list" to form template
- KF-127: /api/timer/settings 404 — Added route alias to api_settings handler
- KF-128: /api/timer/status 401 — Added credentials:'same-origin' to timer fetch calls

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
