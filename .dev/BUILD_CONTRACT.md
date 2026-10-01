# ChipFlow build contract — machine-checkable definition of done

Effective 2026-10-01. Binds every pipeline worker (planner, builder,
verifier, spot-checker, release) on the KanbanFlow-exact-copy rebuild.

**Why this file exists.** The #1 failure mode of long-running agent work
is the agent declaring "code written" as done. Nothing in this project is
DONE until the gates below pass. The agent never judges its own completion —
the gates do. A per-unit contract is negotiated BEFORE code, not after.

Applies to every KF defect fix and every audit follow-up. Tracker Status
lines (`.dev/kf-parity-defects.md`) and `pipe-state.json` are authoritative.

## 1. Per-unit contract (before any code)

For each defect (or a small explicitly-related cluster), the plan phase
produces a spec in this shape — numbered, measurable, naming the golden
master and viewports (KF-250's spec is the template):

1. What changes, with exact expected values (px, colors, text verbatim).
2. Which golden master(s) it is judged against, with viewport(s).
3. The fixture needed to reproduce the state (fixture data, steps).
4. What is NOT in scope (neighboring defects handled separately).
5. Regression expectations: everything that must still hold.

The builder may not start until the contract exists. Ambiguity found during
build goes back to the contract, not into the code silently.

## 2. Layered gates (in order — never proceed with a failed layer)

**L0 — Static.** `cargo fmt --check && cargo build && cargo clippy --all-targets`
(zero warnings) `&& cargo test` (all pass). `node --check static/app.js`.
A failed L0 is a failed unit, full stop.

**L1 — Boot.** The binary runs on a LOCAL test instance: distinct free port,
fresh throwaway redb database under `.dev/test-dbs/`. Never the production
port, never production data. Health responds; page loads with zero console
errors and zero failed network requests.

**L2 — Deterministic visual.** Reproduce the golden master's exact state
(same fixture data, same viewport, devicePixelRatio 1x, fonts settled via
networkidle + fixed delay, animations settled, timers at a deterministic
second). Screenshot both, then run:

```
python3 .dev/pixdiff.py <golden.png> <candidate.png> <diff-out.png>
```

Record the FULL JSON summary (diff_pixels, diff_ratio, max_channel_delta,
diff_regions) in the worker report. PASS = diff_pixels 0, or only
documented-acceptable deviations: subpixel text anti-aliasing (with a zoomed
crop proving it is AA, not a color/size/weight difference) and dynamic data
(timestamps, "x minutes ago", random IDs). **Any 1px or 1-shade deviation
beyond those = FAIL.** There is no "close enough".

**L3 — Adversarial.** A separate verifier that never saw the builder's
explanation runs the blind pass per `.dev/VERIFIER_SPEC.md`, including the
permutation sweep (§3 of `golden-master-verification-standard.md`: empty,
overflow, hover/focus, dialogs, WIP-exceeded, 1440 + 1100 viewports).
A PASS verdict must cite the pixdiff JSON for every permutation.

**L4 — Regression.** The final-battery checks covering the affected area
still pass; no previously-FIXED defect regresses. Tracker entry updated to
FIXED with evidence path; entries are never deleted.

## 3. Geometry rule

When a defect is about the position, size, or color of a KanbanFlow element,
the builder extracts EXACT numbers from the live KanbanFlow DOM
(`getBoundingClientRect()`, `getComputedStyle()`) and transcribes them into
the fix. Vision is for discovery; numbers are for implementation.
Vision-only approximations are not a fix.

## 4. Iteration cap

Max **4 fix attempts per defect per round**. If the verifier still fails
after 4, the defect returns to plan with the full failure log — no silent
re-attempts, no "let me try once more". On return, a second builder may
argue a different cause (challenge-and-converge); the first plausible cause
is not automatically the cause.

## 5. Blast radius

- Test instances only. Kill test servers by exact PID. Never `pkill` chipflow.
- No production ports, no production databases, no secrets, no green-box or
  network changes.
- Deploys only by the release worker under Chip's standing authorization,
  and only after L0–L3 are green, via `.dev/push_via_api.py` with per-commit
  tree verification, dry-activate showing chipflow.service as the sole
  affected unit, and `/api/v1/version` reporting the new SHA.
- Git commits per unit (short imperative message). Never leave the tree in a
  state that blocks the next unit. Tracker + heartbeat updated by the worker
  that did the work.

## 6. State protocol

- Chat output is not the deliverable — files are. Full reports go to
  `.dev/evidence/`; every worker ends its turn with `DONE: <one-liner>` or
  `FAILED: <reason>` and updates the state file it touched.
- Never trust a worker's claim that it updated a state file or advanced a
  cursor. The orchestrator reads the file before treating the step as done.
- Fresh context per unit: builders and verifiers start from the contract +
  defect entry + golden master, never resumed inside a dead worker's context.

## 7. Rollback

Any change that breaks L0–L2 on unrelated areas is reverted before
proceeding. Fix the break first; do not stack fixes on a broken tree.
