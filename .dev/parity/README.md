# Parity harness

Replays KanbanFlow-parity states on a running ChipFlow and compares element
boxes and computed styles with the KanbanFlow goldens. Two tools share one
manifest: `run.mjs` measures against the goldens, `regress.mjs` diffs two
ChipFlow builds against each other.

## Goldens are private and external

The goldens (PNGs and `*.styles.json`) come from the owner's KanbanFlow
account and never enter this repo. They live in `~/Pictures/chipflow-goldens/`.
Set `CHIPFLOW_GOLDENS` to use another folder. `manifest.json` and `entries/`
hold only element keys, selectors, property names and tolerances, never golden
values, except literal numbers read from a golden PNG when its styles file has no
box (marked in the entry note).

## Requirements

- Node 22 or newer and Playwright 1.63 (not a repo dependency). Point
  `PW_NODE_MODULES` at a `node_modules` directory that contains `playwright`
  with its Chromium (installed with `PLAYWRIGHT_BROWSERS_PATH=0`).
- libfaketime (`brew install libfaketime`), `jq`, `curl`, `rg`, ImageMagick
  (`magick`, regression mode only). `start-server.sh` loads
  `/opt/homebrew/lib/faketime/libfaketime.1.dylib`; set `FAKETIME_LIB` to the
  library path on any other install.
- Network on the first run, to fill the CDN cache (`cdn-cache/` next to the
  state file) with htmx and SortableJS. Later runs on that workdir work offline.

## Run

```bash
export PW_NODE_MODULES=/path/to/node_modules
.dev/parity/start-server.sh target/release/chipflow 3420 /tmp/parity-3420
node .dev/parity/run.mjs --state /tmp/parity-3420/state.json --out report.json
kill $(jq -r .pid /tmp/parity-3420/state.json)
```

`start-server.sh` always passes `--database-path` (a fresh redb in the workdir),
creates the admin user, seeds the Golden Master board
(`.dev/tools/seed-golden-master.sh`, `.dev/golden-master-board.json`) and the
Visual Test board (`seed-visual-test.sh`), and writes `state.json`. The server
clock is frozen with libfaketime: the Golden Master board is seeded on
2026-09-30 15:22 and served from 2026-10-16 18:00 (America/Chicago), the day
of the goldens. The browser clock is pinned to the same instant.

`run.mjs` prints a table and the failing comparisons, writes `report.json`
and exits 1 on any failure. Options: `--only id,id`, `--manifest file`,
`--debug-dir dir` (screenshot where an entry broke). `BASE_URL` overrides the
server address in `state.json`. A full run takes about two minutes, most of it
the five timer entries that move the server clock.

The same server can be replayed repeatedly: entries that change server data
declare a `teardown`, and timer entries reset the timer and today's time
entries around themselves. `report.json` holds no timestamps, so two runs on
the same build are byte-identical.

## Regression mode

Start two servers (two ports, two workdirs) from the candidate and baseline
binaries, then:

```bash
node .dev/parity/regress.mjs --candidate /tmp/parity-3421/state.json \
  --baseline /tmp/parity-3420/state.json --out /tmp/regress [--only id,id] [--max-diff N]
```

Each entry is screenshotted on both, the pixels that differ by more than
`--fuzz` (default 1%, about 2 levels per channel, because software
rasterisation jitters by a level on anti-aliased corners between page loads)
are counted, and `diff/<id>.png` marks them in red. Exit status is 1 when any
entry exceeds `--max-diff` (default 0). Prove the two builds differ (binary
hash plus a string unique to the change) before trusting a zero.

Regression mode is a change detector, not a scope check. Any entry that
renders a changed element is flagged, including entries the change was not
about (a new Done "Today" header shows up in every board entry). Read the diff
images to decide whether each change is intended.

The server lists boards in random-id order, so entries that show the board
list sort it first. Anything else that depends on server ordering has to be
made repeatable the same way, or two same-build servers will differ.

## Determinism

Fixed viewport per entry, device scale factor 1, reduced motion, transitions
and animations off, a fresh browser context per entry, htmx and SortableJS
served from a local cache of the CDN, fonts awaited, network idle plus two
animation frames plus a fixed delay. Entries with `"clock": "controlled"`
install a paused browser clock and move the server clock (`ft-<port>.txt`,
re-read by libfaketime about once a second) in step with it through the
`advance` step.

## Manifest

`manifest.json` holds `defaults` and an `include` list of `entries/*.json`.
Each entry:

```json
{"id": "board-03-menu-panel", "golden": "board/03-board-menu-panel",
 "viewport": {"width": 1440, "height": 612}, "clock": "controlled",
 "setup": [{"goto": "/b/{gm}"}, {"click": "#board-menu-btn"}],
 "teardown": [], "comparisons": []}
```

`golden` is the path of a `*.styles.json` under the goldens folder.
`{gm}` and `{vt}` are the Golden Master and Visual Test board ids.

Setup and teardown steps (one key each): `goto`, `click` (with `nth`,
`button`), `dblclick`, `hover`, `contextmenu`, `clickAt` ([x,y], `button`),
`dragTo` ([x,y], `to`, `steps`), `fill`, `select`, `press`, `mouse`,
`waitFor`, `wait`, `eval`, `localStorage`, `reload`, `resize`, `api`
([method, path, body], `{task:vt:Name prefix}` in the body becomes a task id),
`apiColumn` ([board, column name, patch]), `apiTask` ([board, name prefix,
method, suffix, body]), `advance` and `jump` (seconds, controlled clock only).

A comparison:

```json
{"id": "itemBtn", "key": "itemBtn", "sel": "[data-bm=\"filter\"]",
 "props": ["x", "y", "w", "h", "c", "fs"]}
```

- `key`: element label in the golden's `els` list (the part before any `(`).
  `gnth` picks the nth element with that label.
- `from`: borrow the element from another golden file of the same family.
- `sel`, `nth`: Playwright selector (CSS plus `:has-text` and `>>` chains) and
  index.
- `props`: `x y w h right bottom` (viewport CSS px), `c` color, `bg`, `fs`,
  `fw`, `fst`, `lh`, `br`, `b` (four-side border), `p`, `m`, `o`, `text`,
  `scrollW`, `clientW`. An empty list only requires the element to be visible.
- `range`: measure the text-node box, for goldens whose element box includes
  padding.
- `over`: composite a translucent golden color over this backdrop, for example
  `{"bg": "#333333"}`.
- `expect`: literal expected values, replacing or adding to the golden.
- `path`: map a prop to a dotted path in a structured golden (the responsive
  dumps).
- `offset`: `{"x": n, "y": n}` known offset applied to expected x and y. Put
  the reason in the entry note.
- `tol`: override the tolerances.

To add an entry, write its setup, run
`node suggest.mjs --state state.json --entry <id>` to list the ChipFlow element
that overlaps each golden element, then write the comparisons and check them
with `--only`.

## Tolerance policy

Positions and sizes: 1 CSS px (the goldens are rounded to whole pixels).
Colors: exact per channel, 1 per channel when a translucent golden color is
composited. Alpha: 0.02. Font size: 0.1 px. Line height: 0.5 px. Border widths,
padding, margin, radius: 0.5 px. Font weight, style and text: exact (text
matches as a prefix, since the goldens truncate at 40 characters).

Goldens from macOS Chrome are color-managed, so colors come from the styles
files only, never from PNG pixels. A mismatch is a candidate defect, and the
entry id plus comparison id (`entry/comparison.prop`) is its acceptance check.
Raise a tolerance only for a measured, explained rounding difference, and say
why in the entry note.

## Known gaps

- modal/22 (subtask due date) is reachable only if ChipFlow gains the menu
  item: `modal-22-subtask-due-date` checks for it.
- modal/23 and modal/27 have no element list; their popover boxes were read
  from the golden PNGs.
- Dates are not compared as text (the KanbanFlow account time zone is unknown).
- Drag ghost (cards/07) is a native drag image in ChipFlow and cannot be
  measured; the entry is not in the manifest.
