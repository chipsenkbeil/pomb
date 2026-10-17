// Regression mode: screenshot every manifest state on a candidate and a
// baseline ChipFlow and count the pixels that differ.
//
//   PW_NODE_MODULES=<dir with playwright 1.63> node regress.mjs \
//       --candidate CAND_STATE.json --baseline BASE_STATE.json --out DIR \
//       [--manifest manifest.json] [--only id,id] [--max-diff N] [--fuzz 1%]
//
// Start two servers with start-server.sh (two ports, two workdirs) so both
// have the same seed and frozen clock. Writes DIR/candidate/<id>.png,
// DIR/baseline/<id>.png, DIR/diff/<id>.png (differing pixels in red) and
// DIR/report.json. Exit status is 1 when any entry differs by more than
// --max-diff pixels (default 0) or fails to replay. No golden data is used.
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { loadPlaywright, playEntry, readJson, loadManifest, onlyFilter } from './lib.mjs';

const args = Object.fromEntries(
  process.argv.slice(2).reduce((a, v, i, all) => (v.startsWith('--') ? [...a, [v.slice(2), all[i + 1]]] : a), []),
);
const here = path.dirname(fileURLToPath(import.meta.url));
if (!args.candidate || !args.baseline || !args.out) {
  console.error('usage: node regress.mjs --candidate STATE.json --baseline STATE.json --out DIR [--only id,id] [--max-diff N]');
  process.exit(2);
}
const cand = readJson(args.candidate);
const base = readJson(args.baseline);
const manifest = loadManifest(args.manifest || path.join(here, 'manifest.json'));
const selected = onlyFilter(args.only, manifest.entries);
const maxDiff = Number(args['max-diff'] ?? 0);
// Software rasterisation can differ by one or two levels per channel on
// anti-aliased corners between two page loads, so levels up to 1% are equal.
const fuzz = args.fuzz ?? '1%';
const started = Date.now();
for (const d of ['candidate', 'baseline', 'diff']) mkdirSync(path.join(args.out, d), { recursive: true });

// Number of pixels where any channel differs by more than fuzz, and the
// red-on-gray diff image.
function diffPixels(a, b, diffOut) {
  const count = spawnSync('magick', [a, b, '-compose', 'difference', '-composite', '-separate',
    '-evaluate-sequence', 'max', '-threshold', fuzz, '-format', '%[fx:round(mean*w*h)]', 'info:'], { encoding: 'utf8' });
  if (count.status !== 0) throw new Error(`magick: ${count.stderr}`);
  spawnSync('magick', ['compare', '-metric', 'AE', '-fuzz', fuzz, '-highlight-color', 'red', '-lowlight-color', 'gray90', a, b, diffOut]);
  return Number(count.stdout);
}

const size = (f) => spawnSync('magick', ['identify', '-format', '%wx%h', f], { encoding: 'utf8' }).stdout;

const pw = loadPlaywright();
const browser = await pw.chromium.launch();
const report = { candidate: cand.base, baseline: base.base, chromium: browser.version(), entries: [] };

for (const entry of manifest.entries) {
  if (!selected(entry.id)) continue;
  const rec = { id: entry.id, status: 'same', diffPixels: null };
  report.entries.push(rec);
  if (entry.skip) {
    rec.status = 'skipped';
    continue;
  }
  const shots = {};
  for (const [name, st] of [['candidate', cand], ['baseline', base]]) {
    const file = path.join(args.out, name, `${entry.id}.png`);
    const res = await playEntry(browser, st, st.base, entry, (page) => page.screenshot({ path: file }));
    if (res.error) {
      rec.status = 'error';
      rec.error = `${name}: ${res.error}`;
      break;
    }
    shots[name] = file;
  }
  if (rec.status === 'error') continue;
  if (size(shots.candidate) !== size(shots.baseline)) {
    rec.status = 'size';
    rec.sizes = [size(shots.candidate), size(shots.baseline)];
    continue;
  }
  rec.diffPixels = diffPixels(shots.candidate, shots.baseline, path.join(args.out, 'diff', `${entry.id}.png`));
  if (rec.diffPixels > maxDiff) rec.status = 'differs';
}
await browser.close();

console.log(`${'ENTRY'.padEnd(34)} ${'STATUS'.padEnd(8)} DIFF PIXELS`);
let bad = 0;
for (const r of report.entries) {
  if (!['same', 'skipped'].includes(r.status)) bad++;
  console.log(`${r.id.padEnd(34)} ${r.status.padEnd(8)} ${r.diffPixels ?? ''}${r.error ? ' ' + r.error : ''}${r.sizes ? ' ' + r.sizes.join(' vs ') : ''}`);
}
console.log(`\n${report.entries.length} entries, ${bad} differ or failed; ${((Date.now() - started) / 1000).toFixed(1)}s`);
report.summary = { entries: report.entries.length, differing: bad, maxDiff };
writeFileSync(path.join(args.out, 'report.json'), JSON.stringify(report, null, 1) + '\n');
process.exit(bad ? 1 : 0);
