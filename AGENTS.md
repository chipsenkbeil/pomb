# AGENTS.md — ChipFlow

## What this is

ChipFlow: a self-hosted kanban board with pomodoro timer and time tracking.
Single Rust binary (axum + Askama + htmx + SortableJS), pure-Rust `redb`
storage, no SQL, no migrations, no Docker required.

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

- **Pure Rust only.** No SQLite, no SQL, no C dependencies, no migrations.
- **Columns and swimlanes are runtime-customizable.** Never bolt behavior
  to seeded column/swimlane names — resolve the Done column (etc.) from
  runtime configuration.
- Auth: browser session cookie for the UI; `Authorization: Bearer cf_...`
  API tokens for programmatic access (see `/agents.md`). Token management
  endpoints require the cookie — a token must never mint new tokens.
- Keep `agents.md`, `agents/skill.md`, and `agents.json` accurate when API
  behavior changes — agents consume them directly. The OpenAPI spec at
  `/api/v1/openapi.json` is generated from the `#[utoipa::path]` annotations
  and `ToSchema` view structs in `src/routes.rs` — update those, not a file.
- Single-admin model. Keep the schema mappable to org-mode concepts
  (future org-agenda/org-roam integration); don't add fields that fight that.
- Commit messages: short imperative summary, no fluff.
