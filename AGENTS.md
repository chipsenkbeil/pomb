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
  switched green-box, the worker runs `dev verify-prod`, which confirms the
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
| `dev verify-prod <sha> --batch N --backup <file> --kf 570,571` | read-only post-switch checks on green-box (build_sha, app.js, cross-site 403, journal, `/login`, OpenAPI server URL, `token_invalid`); on success prepends the RELEASED status lines, commits, pushes and prints the counts (`--no-tracker` checks only, `--cleanup <worktree>...` removes worktrees and merged branches) |

`dev lint` and `dev test` exit 0 before a change is done. Rust style follows
`docs/STYLE.md`. `.cargo/config.toml` sets `TZ=UTC` for the test run.

Release embeds `static/` via rust-embed; debug serves it from disk.

## Architecture notes

- `src/main.rs`: CLI (`--auth`/`--mode`/`--host`/`--port`/`--database-path`/`--backup-dir`/`--public-url`/`--trust-proxy`, env-overridable), startup.
- `src/routes.rs`: all routes + handlers + Askama templates wiring.
- `src/routes/slot.rs`: the slots that let a few attachment uploads, downloads and whole-board reads (the board page and the calendar feed) run at once, each request holding one of its account's slots (taken after the task or board is resolved to that account) and one of the server's, and `SlotBody`, the response body that frees its slots when it is sent or after 120 seconds. `src/routes/export.rs`: `TaskStream`, the board export body that sends one task at a time.
- `src/db.rs`: redb tables (`*_by_*` multimaps are secondary indexes). Every board, token,
  saved template and time entry belongs to an account; `src/db/` holds the three handles the
  routes reach data through: `AccountDb` (one account's rows, by `Owned*` proofs), `InstanceDb`
  (instance settings, invites and the account list, no board content) and `PublicDb` (sign-in, setup, invite
  registration, recovery, device pairing, the calendar feed). `src/db/membership.rs` holds the
  instance mode, invites, registration and its approval, suspension, recovery codes and account
  deletion, `src/db/activity.rs` when each account was last seen and each board last used, and
  `src/db/prune.rs` the daily deletion of unused member boards and the accounts it leaves empty, and `src/db/usage.rs` the bytes each account stores, which every task, template and attachment write keeps in step through `txn_store_task`. `src/db/entries.rs` indexes each account's time entries in the multimaps `entries_by_account` and `entries_by_day` (keyed `account|UTC day`), which every entry write keeps in step through `txn_store_entry` and `txn_remove_entry`, and holds the time entry quota and the per-day cap. The multimap `passkeys_by_user` indexes each account's passkeys (at most 10, recovery excepted), the table `passkeys_by_credential` maps each credential id to its passkey, the multimaps `tokens_by_account`, `sessions_by_user` and `devices_by_account` index each account's tokens (live and suspended), sessions and approved device requests, so registration's duplicate check and account deletion never scan those tables, the table `token_ids_by_hash` maps the SHA-256 of each API token (live or suspended) to its id so a Bearer request never scans the tokens, the multimap `salted_tokens_by_last4` finds a token that predates it and still holds a salted hash (it is keyed the first time it authenticates), the multimap `devices_by_user_code` finds a device request by its user code, and the table `feed_tokens` maps the SHA-256 of each calendar-feed token to its board, kept in step by every board write through `txn_store_board`.
- `src/disk.rs`: `DiskGuard`, which stops writes that grow the database while it or its file holds more than its size limit, the disk is below its free space floor, or no measurement is younger than 60 seconds. It measures every 10 seconds on a blocking thread, a write that finds the sample older starts one more there, and no request ever measures or takes the writer. The `Writes` flag of each `ROUTE_ACCESS` entry says which routes it stops. Quotas, rate limits and the `DiskLimits` are `InstanceSettings` fields the admin edits.
- `src/backup.rs`: consistent database snapshots (a held write transaction), rotation and the schedule; the settings and last status are `BackupSettings` and `BackupStatus` rows in the `settings` table.
- `src/auth.rs`: sign-in modes (`--auth none|passkey`), session cookies, API tokens. The table `sessions` is keyed by the SHA-256 of the cookie value, so a backup holds no value that signs a browser in; session creation refuses an absent or suspended account inside its write transaction, and finishing recovery revokes the account's tokens, other passkeys and sessions, and denies its approved device requests, in the one that commits it.
- `src/passkey.rs`: the WebAuthn relying party (webauthn-rs) and its pending ceremonies.
- `src/models.rs`: shared domain types.

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
