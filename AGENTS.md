# AGENTS.md — Pomb

## What this is

Pomb: a self-hosted kanban board with pomodoro timer and time tracking.
Single Rust binary (axum + Askama + htmx + SortableJS), pure-Rust `redb`
storage with an additive schema guarded by `SCHEMA_VERSION`, no SQL, no
Docker required.

## Work board

The Pomb board "pomb" (`dev board`) is the single list of work. KanbanFlow
parity gaps carry the `kf-parity` label, and each card holds its done-when
outcomes. Release pipeline: build in a worktree, the orchestrator inspects the
diff, a different worker verifies blind, `dev land` merges it into main and
moves the card to Release, then `dev release`, the homelab switch,
`dev verify`, and the Release cards move to Done. Contracts: `.dev/BUILD_CONTRACT.md`
(L0-L4 gates) and `.dev/VERIFIER_SPEC.md` (adversarial evidence protocol);
fixture `.dev/golden-master-board.json`.

## Build & check

Run everything through `dev`, which executes the scripts in `.dev/`. Those
scripts are the sanctioned entry point: do not call `cargo fmt`, `cargo clippy`
or `cargo test` directly, and do not edit `.dev/lint.lua` above its
`-- --- project-specific checks below ---` marker (`dev sync` owns that part).

| Command | Runs |
|---|---|
| `dev lint` | nightly `cargo fmt --check`, clippy with `-D warnings` (and a release pass when the source mentions `debug_assertions`), `ast-grep scan` and `ast-grep test`, `cargo sort --check`, `cargo deny check -W unmaintained`, the manifests check, the source checks (derive order, module docs, cfg imports, comment periods and runs, getters, doc headings, comment spacing, unsafe), then the project checks: `node --check` on `static/app.js`, `static/passkey.js`, `static/pages.js` and `static/docs.js`, no inline scripts, no C build tools in the dependency tree, `shellcheck` and `shfmt` on `.dev/site` scripts, private details, and parity history entries |
| `dev test` | `cargo nextest run --all-features` (it takes any arguments), then `cargo test --doc --all-features` (only without arguments), then each `tests/js/*.js` with `node` against `static/app.js` |
| `dev fmt` | `cargo +nightly fmt` with `rustfmt.toml` |
| `dev build` | `cargo build --release` |
| `dev land <commit> --card ID` | refuses a commit that is not exactly one commit past main on the remote of record, which mirrors to GitHub, or that changes the version, applies it onto main, refuses attribution, runs `dev lint --remote` then `dev test --remote` on ci, pushes without forcing (applying it again on the new main when main moved, 3 attempts in all), notes the sha on the card, moves the card to Release, then removes the card's worktree and branch (and in jj its workspace and bookmark) |
| `dev release --batch N --summary "..."` | refuses unless main on the remote of record has a commit since its last `v*` tag, bumps the version in `.dev/worktrees/release-N` and commits `Release batch-N: ...` with only `Cargo.toml` and `Cargo.lock`, runs `dev lint --remote`, `dev test --remote`, `dev nixcheck --remote` and `dev build --remote` one at a time on ci, checks for attribution, refuses when main moved, pushes main, tags `batch-N` and `vX.Y.Z`, then prints the sha, nix hash and condition for the homelab request (`--hash-only <sha>` reprints the block for a pushed sha, `--env KEY=VALUE` lists service env changes, `--dry-run` skips the push and hash) |
| `dev nixcheck` | builds a nextest archive, then runs the tests in a user, mount and network namespace with no `/usr/bin/env` and only the tools the Nix derivation provides on PATH, so a missing tool fails before release. Linux only |
| `dev regress [REV]` | diffs this tree's screenshots against REV (default main on the remote of record) with the ci `regress` job on a browser runner and keeps the report as an artifact and prints the run id and the `ci cp` command that fetches it |
| `dev verify <sha> --backup <file>` | read-only post-switch checks on green-box (build_sha, app.js, cross-site 403, journal, `/login`, OpenAPI server URL, `token_invalid`; `--cleanup <worktree>...` removes worktrees and merged branches, `--delete-branch <branch>` also deletes an unmerged one) |

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
  Token and passkey management endpoints require the browser. A token never mints a token, with
  one exception (the owner approved it on 28 October and the two-level rules on 3 November): a
  token holding `tokens.issue` (the lead preset, the admin preset and the lead role include it)
  creates child tokens through
  `POST /api/v1/auth/tokens/children`, and revoking the issuer revokes them. A child holds a
  subset of its issuer's permissions and boards. A root token is one made in the browser or by
  device approval. A child request that sets `preset` or `permissions` may give `tokens.issue`
  only when its issuer is a root token. A child created with `role_id` gets the role's
  permissions that its issuer also holds, and never `tokens.issue`. A child of a child never
  holds `tokens.issue`, so the tree stops at two levels. An issuer's `child_lifetime_hours`
  is 1 to 8, default 1, set when the owner creates it in the browser, and no edit changes it. A
  child's `expires_in_hours`
  defaults to 1 and may not exceed the issuer's `child_lifetime_hours` (the API answers 400
  above it), and a child never expires after its issuer. Child tokens do not count toward an
  account's 50 tokens and have no count limit. An issuer gets 429 `rate_limited` after a burst
  of 20 child creations, then one more every 2 seconds. A token holding `tokens.issue` also
  lists and revokes its own children over Bearer, and no other token's children. Any token, with no permission, revokes
  itself and every token it issued with `POST /api/v1/auth/tokens/revoke-self` over Bearer, and
  a browser session is refused there. A token never
  adds a passkey. Only the owner edits, renames or deletes an agent role, in the browser. A
  Bearer token only creates the standard roles the account lacks and grants or revokes a role on
  boards it holds. A token acts as a role when it is issued from the Settings form or by device
  approval, or as the child of a token holding `tokens.issue` made with `role_id`. A token edit
  changes only the name, so a permission, board or lifetime change is a revoke and a new token. Browser writes (every non-Bearer write, with or without a cookie) must be
  same-origin (`src/csrf.rs`, the router's outermost layer), and Bearer requests skip that
  check.
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
- Commit messages: short imperative summary, no fluff, no attribution and no trailers except `Change-Id:`.
