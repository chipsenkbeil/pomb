# ChipFlow handoff: state and remaining work (2026-10-16)

ChipFlow is a self-hosted kanban board with a pomodoro timer and time
tracking, built as a pixel-close copy of KanbanFlow. It is a single
Rust binary (axum, Askama templates, htmx, SortableJS) with pure-Rust
redb storage. No SQL, no migrations, no Docker needed.

This file is the starting point for finishing the project in Muse
Code. Read it, then `AGENTS.md` (the standing rules for this repo), then
`.dev/LAPTOP-VALIDATION.md` (how to check work on a laptop).

## Where it stands

- Production is live at https://chipflow.chip.network and runs build
  `3711331` (full SHA `371133161bf32d98ed42e301a4c1e3858de27683`),
  deployed 2026-10-16. That build shipped the KF-293 statistics
  tooltip and bar-label fix. Later commits on `main` are tracker and
  handoff material only, so production still matches `3711331`.
- The defect tracker (`.dev/kf-parity-defects.md`) counts, by the
  counting tool in `.dev/tools/tracker_count.py`: OPEN 1, RELEASED 109,
  DEPLOYED 0, INVALID 2. FIXED 56 and CLOSED 2 are tallied separately
  and are not part of the counts above.
- The one OPEN defect is KF-331 (board settings page chrome). It is
  built but not verified and not released. See item R1 below.
- Reference boards: KanbanFlow at
  https://kanbanflow.com/board/JYv2f3u (Chip's account), and its
  ChipFlow mirror "ChipFlow Golden Master", board id
  `c9326edd-856b-4608-897a-077c3a387572`, at
  https://chipflow.chip.network/b/c9326edd-856b-4608-897a-077c3a387572

## How the repo is organized

- `src/main.rs` holds startup and the command line flags.
- `src/routes.rs` holds all routes, handlers, and template wiring.
- `src/db.rs` holds the redb tables and queries.
- `src/models.rs` holds the shared domain types.
- `src/auth.rs` holds password auth, session cookies, and API tokens.
- `static/` holds the JavaScript and CSS. Release builds embed it with
  rust-embed, and debug builds serve it from disk.
- `templates/` holds the Askama templates.
- `agent-guide.md`, `agents/skill.md`, and `agents.json` describe the API
  for outside agents. Keep them accurate when API behavior changes.
  The OpenAPI spec at `/api/v1/openapi.json` is generated from the
  annotations in `src/routes.rs`, so change those, not a file.
- Repo note: the API guide file is `agent-guide.md` (renamed 2026-10-16
  from `agents.md` because the pair `AGENTS.md`/`agents.md` collided on
  case-insensitive filesystems); it is still served at GET /agents.md.
- `.dev/kf-parity-defects.md` is the single list of every parity gap.
  The parse contract in `AGENTS.md` is authoritative. The short form:
  the first `### KF-NNN` section wins when an id appears twice, the
  first `- Status:` line inside a section is the current state, newer
  status lines are prepended, and existing status lines are never
  edited or deleted.
- `.dev/BUILD_CONTRACT.md` defines what done means for a unit of work
  (a written contract first, then layered checks in order).
- `.dev/VERIFIER_SPEC.md` is the evidence protocol for verification.
  Read it before verifying anything.
- `.dev/evidence/` holds the proof files: build notes, verifier
  reports, deploy reports, and screenshots. 307 files at handoff time.
  Name new files with the defect id and the date.
- `.dev/tools/` holds the helper scripts: a tracker counter, a visual
  audit for a local test instance, and the setup script the audit
  needs. `.dev/LAPTOP-VALIDATION.md` explains each one.
- `.dev/golden-master-2026-09-30.md` and
  `.dev/golden-master-board.json` describe the fixture board that the
  KanbanFlow board and the ChipFlow mirror are both seeded from.

## What is lost

Be aware of this before trusting old notes:

- The original golden-master verification standard document, the
  golden inventory document, and the original golden-master image set
  were never committed to this repo and no longer exist anywhere.
  `AGENTS.md` and `.dev/VERIFIER_SPEC.md` still point at the standard
  document. Treat those pointers as stale. The method notes inside
  `.dev/VERIFIER_SPEC.md` itself are intact and usable.
- The pixel-diff tool that produced the old `pixdiff` reports is also
  gone. Only its past JSON outputs survive, inside `.dev/evidence/`.
- What survives as visual reference: the two fixture documents above,
  13 recovered KanbanFlow captures in `.dev/evidence/golden-masters/`
  (see the README there for provenance), the recovered board-settings
  capture used for KF-331, and the evidence files.
- A historical inventory from the lost document claimed 171 checked
  rows: 97 pixel, 54 text-only, 15 none, 5 not applicable. Re-derive
  the inventory before trusting those numbers.
- An old result of "0 of 57 screenshots pass" is invalid. The board
  framing in those captures did not match, so the failures say
  nothing about the app. Do not repeat that run as it was designed.

## Working rules that produced good results

- The worker that builds a fix never verifies it. Verification is a
  separate pass, done blind where possible, and it must attach
  evidence (commands run, measurements, screenshots).
- A tracker status line is not evidence. Before calling a fix
  released, confirm the fix content is present in the deployed tree
  or in the assets the production server actually serves. Tracker
  lines have been wrong before, most notably KF-293, whose first
  deployed marker claimed a fix that was not in the tree.
- Advance a defect only by prepending a new `- Status:` line to its
  section. Never edit or delete an older line.
- Do build and test work in worktrees under `.dev/worktrees/`. Never
  build inside `/tmp` (it is a small tmpfs on the old build VM and
  builds died there).
- Columns and swimlanes are set at run time by the board owner.
  Never tie behavior to seeded column or swimlane names.
- Commit messages are short imperative summaries with no fluff.

## The release procedure

Follow these steps in order. The procedure pushes to `main` on
GitHub, then deploys to the home server (green-box) by pin update.

1. Fetch: `git fetch origin`.
2. Make a release worktree under `.dev/worktrees/` from the current
   `origin/main`.
3. Cherry-pick the verified fix commit onto it. The tree must contain
   exactly the verified change and nothing else.
4. Run the checks in the worktree and stop if any fail:
   `cargo fmt --check`, `cargo build`,
   `cargo clippy --all-targets` (zero warnings), `cargo test`, and
   `node --check static/app.js`.
5. Push `main`. On Dale's build VM this used the Git Data API script
   `.dev/push_via_api.py` with the worktree path as an absolute path
   and the local commit's parent as the base argument. The script
   first checks that the remote tree matches the base tree. The
   script reads its GitHub credential through a helper that only
   exists on that VM, so on Chip's laptop push the same commit with a
   plain `git push origin main` instead.
6. On green-box, open the homelab repo and update only the ChipFlow
   pin in `modules/chipflow.nix` (full path
   `~/homelab-dale-chipflow/modules/chipflow.nix`): set
   the rev to the new `main` commit and the hash to the SRI hash of
   that commit's source tarball, which `nix store prefetch-file
   --unpack` prints for the tarball.
7. Still on green-box, run
   `nixos-rebuild dry-activate --flake .#green-box` and confirm that
   `chipflow.service` is the only unit that changes.
8. Run `nixos-rebuild switch --flake .#green-box`.
9. Verify the deploy: `curl -s http://127.0.0.1:3000/api/v1/version`
   on green-box and `curl -s
   https://chipflow.chip.network/api/v1/version` from outside both
   report the new build SHA.
10. Verify the fix itself in what production serves, not just the
    version number. Example: compare the sha256 of the served
    `static/app.js` with `git show <sha>:static/app.js`.
11. Close out in the tracker: prepend a RELEASED status line to the
    defect section naming the production SHA and the evidence files,
    add any new evidence files, commit, and push that commit the
    same way as step 5.

Safety rules for green-box and production:

- Production is the service on port 3000 only. Port 3100 holds a
  stale test instance from September. Never treat it as production.
- Never run `pkill -x chipflow` or `pkill -f` on green-box. The
  first also kills the test instance, and the second can match the
  SSH session that carries your own command and kill it.
- Do not disturb Caddy, Tailscale, the Funnel, or ports 443 and
  8443.
- Never edit `secrets/green-box.yaml` by hand. It breaks the sops
  MAC. Change keys with `sops` on green-box itself.
- Dale's VM holds a local `master` branch and a working tree that
  are stale and diverged. `origin/main` is the only truth.

## Remaining items

R1. Verify and release KF-331 (board settings page chrome).

The fix is built on the branch `handoff/batch-30-kf331`, tip
`77a6537`, and it is not on `main`. It changes only
`static/style.css` and `templates/board_settings.html`. The build
evidence is in `.dev/evidence/kf331-batch30-build-20261016.md` and
the mismatch proof is in
`.dev/evidence/kf331-gm134-recovery-20261016.md`, against the
reference capture
`.dev/evidence/kf331-kanbanflow-settings-colors-recovered-20261016.png`.
On the branch base the checks were green: fmt check, node check, and
96 tests passing with none failing.

Next actions: verify the branch in a fresh pass that is not the
builder. Re-measure the settings page on a test instance (page
background `#d3e8f2`, one 900 px square-cornered container, shadow
edge profile, header band `#e8e8e8`, nav `#f4f4f4` at 240 px) and
confirm the user settings page is unchanged, because the build
scoped its CSS to the board settings page for exactly that reason.
Judge the builder's three open calls:

- Nav row height renders at 34 px where the reference pitch is
  about 39 px. The builder left text metrics alone on purpose.
- The content elements inside the Colors tab kept their existing
  styling. The defect covers the container chrome, not the content.
- KanbanFlow's dark global top bar was not reproduced. It is app
  chrome in the reference, not settings chrome.

Then release the branch by the procedure above (cherry-pick
`77a6537` in a release worktree) and close out KF-331 in the
tracker with a RELEASED line and the verification evidence.

R2. Run the final readiness battery before Chip reviews the app.

The old battery cannot be rerun as designed, because the inventory
and most goldens are lost (see "What is lost"). Next actions:
re-derive the checklist of states and surfaces from the tracker and
`.dev/golden-master-2026-09-30.md`, run the pixel comparisons the
13 recovered captures support, cover the states that were never
tested, run a full pass over the app's functions and interactions,
and finish with a side-by-side comparison of the two reference
boards (KanbanFlow `JYv2f3u` against the ChipFlow Golden Master
mirror). Keep framing identical between paired captures, and record
viewport, device pixel ratio, instance port, and database identity
for every comparison.

R3. Align the fixture board's logged time.

`.dev/golden-master-board.json` says the task "Build payment
webhook handler" has 2.5 logged hours. The live KanbanFlow board
also received an accidental entry of about 3 minutes and a
deliberate completed pomodoro of 10 minutes on that task, and
nothing shows they were removed. Next actions: either restore the
KanbanFlow board to the fixture values, or update the fixture and
reseed every mirror. Then verify the exact logged values on both
boards, since the board comparison in R2 depends on them matching.

R4. Known accepted gaps. Decide on these rather than rediscovering
them:

- The board history report cannot show deletions, because ChipFlow
  does not log them. Showing them would need storage changes.
- Dark mode has no dark styling for column bodies, so they stay
  white in dark mode. This predates the batch-28 work and was left
  as observed.
- Monte Carlo forecasts vary from run to run. The simulation is
  unseeded by design.

R5. Chip's final visual sign-off, then the stable tag.

Blocked on Chip. Do not tag before he has reviewed the app himself.

R6. Housekeeping only Chip can do.

A production-board seeding API token is still valid on the server.
The local copy was discarded, so nothing on the old build VM can
use it. Revoke it in ChipFlow Settings if it should not stay valid.

## First session

1. Read `AGENTS.md`, this file, and `.dev/LAPTOP-VALIDATION.md`.
2. Clone or pull `main` and run the check suite from the runbook.
   Expect every check green before changing anything.
3. Start on R1: pull the branch `handoff/batch-30-kf331` and
   verify the KF-331 build.
