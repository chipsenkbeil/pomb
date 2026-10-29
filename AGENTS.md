# AGENTS.md — Pomb

## What this is

Pomb: a self-hosted kanban board with pomodoro timer and time tracking.
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
  delete existing lines). Push tracker-only changes with `dev push` from a
  branch on top of the current `main`, never a direct push of a diverged
  branch.
- Pipeline: builder fixes in an isolated worktree → the orchestrator
  inspects every diff → a DIFFERENT worker verifies blind → the release
  worker runs `dev release` to land the verified diff as one squashed
  commit on top of `main` (it also refuses attribution trailers before the
  push) and prints the homelab request; once the homelab session has
  switched green-box, the worker runs `dev verify`, which confirms the
  fix content is live (tracker status lines alone are not evidence) and
  only then prepends the RELEASED lines.
- Contracts: `.dev/BUILD_CONTRACT.md` (L0–L4 gates),
  `.dev/VERIFIER_SPEC.md` (adversarial evidence protocol), goldens under
  `.dev/evidence/golden-masters/`, fixture `.dev/golden-master-board.json`.

## Build & check

Run everything through `dev`, which executes the scripts in `.dev/`. Those
scripts are the sanctioned entry point: do not call `cargo fmt`, `cargo clippy`
or `cargo test` directly, and do not edit `.dev/lint.sh` above its
`# --- project-specific checks below ---` marker (`dev sync` owns that part).

| Command | Runs |
|---|---|
| `dev lint` | nightly `cargo fmt --check`, clippy with `-D warnings`, ast-grep, `cargo sort --check`, `cargo deny check bans`, the manifest, derive-order, module-doc and comment-spacing checks, then `node --check static/app.js` |
| `dev test` | `cargo nextest run --all-features`, then `cargo test --doc` |
| `dev fmt` | `cargo +nightly fmt` with `rustfmt.toml` |
| `dev build` | `cargo build --release` |
| `dev release <commit> --batch N --summary "..."` | applies a verified commit to origin/main in `.dev/worktrees/release-N`, commits `Release batch-N: ...`, runs `dev lint --remote`, `dev test --remote` and `dev build --remote` one at a time on ci, checks for attribution, pushes to main, then prints the sha, nix hash (300 second limit, three attempts) and condition for the homelab request (`--hash-only <sha>` reprints the block for a pushed sha without touching git, `--env KEY=VALUE` lists service env changes, `--dry-run` skips the push and hash) |
| `dev verify <sha> --batch N --backup <file> --kf 570,571` | read-only post-switch checks on green-box (build_sha, app.js, cross-site 403, journal, `/login`, OpenAPI server URL, `token_invalid`); on success prepends the RELEASED status lines, commits, pushes and prints the counts (`--no-tracker` checks only, `--cleanup <worktree>...` removes worktrees and merged branches) |

`dev lint` and `dev test` exit 0 before a change is done. Rust style follows
`docs/STYLE.md`. `.cargo/config.toml` sets `TZ=UTC` for the test run.

Release embeds `static/` via rust-embed; debug serves it from disk.

## Architecture notes

`docs/ARCHITECTURE.md` maps the modules. Agents update it with the change that adds, moves or removes a module, without asking.

## Rules

- **Pure Rust only.** No SQLite, no SQL, no C dependencies. The schema is
  additive and guarded by `SCHEMA_VERSION` in `src/db.rs`: bump it whenever
  a stored struct or table gains a field or table, so older binaries refuse
  the newer file. The `stored_schema_should_match_fingerprint` test hashes stored
  field names and types plus table names and key/value types, and fails
  until you bump it and append the fingerprint it prints. Add any new
  stored type to that test's type list, or its changes go unchecked.
- **Columns and swimlanes are runtime-customizable.** Never bolt behavior
  to seeded column/swimlane names — resolve the Done column (etc.) from
  runtime configuration.
- Auth: no passwords. `--auth none` (default) treats every browser request
  as the owner's and only starts on a loopback `--host`; `--auth passkey`
  needs `POMB_PUBLIC_URL` and signs the owner in with a passkey, which sets
  the browser session cookie. `Authorization: Bearer pomb_...` API tokens
  work in both modes (see `/skill.md` and `/docs/api.md`). `--mode multi` serves the admin plus
  invited or approved members, needs `--auth passkey`, and puts its routes (invites,
  registration, recovery, account deletion, the admin page and `/pending`) behind multi mode, so
  single mode answers them 404. A pending account reaches only `/pending` and sign-out.
  Token and passkey management endpoints require the browser, and a token
  must never mint new tokens or add a passkey.
  Browser writes (every non-Bearer write, with or without a cookie) must be
  same-origin (`src/csrf.rs`, the router's outermost layer); Bearer requests
  skip that check.
- API prose lives in the utoipa annotations, `skill.md` covers workflow only,
  and the doc tests enforce both. The OpenAPI spec at `/api/v1/openapi.json`
  and the reference at `/docs/api.md` are generated from the
  `#[utoipa::path]` annotations, the tag descriptions and the `ToSchema` view
  structs in `src/routes.rs`, so update those. The skill (`/skill.md`) and the
  `/llms.txt` index live in `src/routes/docs/`. Any change to the skill text
  bumps `SKILL_VERSION` in `src/routes/docs.rs` and its `metadata.version`,
  and appends the fingerprint the skill test prints.
- **Data access is bound to its owner.** Handlers in `src/routes.rs` never touch
  the raw `Db` or `state.db` (the `routes-raw-db` ast-grep rule refuses both):
  they take an `AccountDb`, `InstanceDb` or `PublicDb` extractor, load every id
  from the path, body or query through an `AccountDb` getter, and pass the
  proof it returns to the mutation. Classify every new route in
  `ROUTE_ACCESS`; the cross-account matrix test then attacks it.
- Single mode: one account, the admin. Keep the schema mappable to org-mode
  concepts (future org-agenda/org-roam integration); don't add fields that
  fight that.
- Commit messages: short imperative summary, no fluff, no trailers or attribution.
