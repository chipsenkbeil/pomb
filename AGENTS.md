# AGENTS.md — ChipFlow

## What this is

ChipFlow: a self-hosted kanban board with pomodoro timer and time tracking.
Single Rust binary (axum + Askama + htmx + SortableJS), pure-Rust `redb`
storage with an additive schema guarded by `SCHEMA_VERSION`, no SQL, no
Docker required.

## Parity tracker — pick up work here

`.dev/kf-parity-defects.md` is the single authoritative list of every
KanbanFlow-parity gap (visual defects and unimplemented features alike —
parity means everything implemented). It moved here on 2026-10-09; the old
TrueNAS `chipflow-tracker.git` repo is retired, do not read or write it.

- Each defect is a `### KF-NNN` section. The AUTHORITATIVE state is the
  FIRST `- Status:` line after the header (newest lines are PREPENDED; the
  `[bracket]` in the header goes stale). Stop scanning at the next `###`.
  Anchor the state match at the start of the line; match generically and
  classify afterward (sections starting FIXED/CLOSED/INVALID or with no
  status line are excluded from counts).
- To advance a defect, PREPEND a new `- Status:` line (never edit or
  delete existing lines). Push tracker-only changes with
  `.dev/push_via_api.py` on top of the current `main` — same safe path as
  code releases, never a direct push of a diverged branch.
- Pipeline: builder fixes in an isolated worktree → Dale inspects every
  diff → a DIFFERENT worker verifies blind → release worker applies the
  verified diff onto the release lineage via the Git Data API and confirms
  the fix content is present in the deployed tree (tracker status lines
  alone are not evidence).
- Contracts: `.dev/BUILD_CONTRACT.md` (L0–L4 gates),
  `.dev/VERIFIER_SPEC.md` (adversarial evidence protocol),
  `.dev/golden-master-verification-standard.md` + goldens under
  `.dev/evidence/golden-masters/`, fixture
  `.dev/golden-master-board.json`.

## Build & check

```bash
cargo fmt --check && cargo build && cargo clippy && cargo test
node --check static/app.js
```

Release embeds `static/` via rust-embed; debug serves it from disk.

## Architecture notes

- `src/main.rs` — CLI (`--host`/`--port`/`--database-path`, env-overridable), startup.
- `src/routes.rs` — all routes + handlers + Askama templates wiring.
- `src/db.rs` — redb tables (`*_by_*` multimaps are secondary indexes).
- `src/auth.rs` — argon2 password auth, session cookies, API tokens.
- `src/models.rs` — shared domain types.

## Rules

- **Pure Rust only.** No SQLite, no SQL, no C dependencies. The schema is
  additive and guarded by `SCHEMA_VERSION` in `src/db.rs`: bump it whenever
  a stored struct or table gains a field or table, so older binaries refuse
  the newer file. The `stored_schema_matches_fingerprint` test hashes stored
  field names and types plus table names and key/value types, and fails
  until you bump it and append the fingerprint it prints. Add any new
  stored type to that test's type list, or its changes go unchecked.
- **Columns and swimlanes are runtime-customizable.** Never bolt behavior
  to seeded column/swimlane names — resolve the Done column (etc.) from
  runtime configuration.
- Auth: browser session cookie for the UI; `Authorization: Bearer cf_...`
  API tokens for programmatic access (see `/agents.md`). Token management
  endpoints require the cookie — a token must never mint new tokens.
- Keep `agent-guide.md`, `agents/skill.md`, and `agents.json` accurate when API
  behavior changes — agents consume them directly. The OpenAPI spec at
  `/api/v1/openapi.json` is generated from the `#[utoipa::path]` annotations
  and `ToSchema` view structs in `src/routes.rs` — update those, not a file.
- Single-admin model. Keep the schema mappable to org-mode concepts
  (future org-agenda/org-roam integration); don't add fields that fight that.
- Commit messages: short imperative summary, no fluff.

## Handoff (2026-10-16)

Chip is finishing the remaining work with Muse. Read
`.dev/HANDOFF.md` first for current state, remaining items, and the
laptop validation runbook (`.dev/LAPTOP-VALIDATION.md`).
