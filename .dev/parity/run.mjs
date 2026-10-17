// Replay every manifest entry against a running ChipFlow and compare element
// geometry and styles with the KanbanFlow goldens.
//
//   PW_NODE_MODULES=<dir with playwright 1.63> node run.mjs --state STATE.json \
//       [--manifest manifest.json] [--out report.json] [--only id,id] \
//       [--debug-dir DIR]
//
// BASE_URL overrides the server address in STATE.json. Exit status is 1 when
// any comparison fails, any entry errors or the console logs an error.
// See README.md.
import path from 'node:path';
import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { loadPlaywright, playEntry, readJson, goldensDir, loadManifest } from './lib.mjs';
import { runComparison } from './measure.mjs';

const args = Object.fromEntries(
  process.argv.slice(2).reduce((a, v, i, all) => (v.startsWith('--') ? [...a, [v.slice(2), all[i + 1]]] : a), []),
);
const here = path.dirname(fileURLToPath(import.meta.url));
if (!args.state) {
  console.error('usage: node run.mjs --state STATE.json [--manifest F] [--out report.json] [--only id,id] [--debug-dir DIR]');
  process.exit(2);
}
const state = readJson(args.state);
const baseUrl = process.env.BASE_URL || state.base;
const manifest = loadManifest(args.manifest || path.join(here, 'manifest.json'));
const outFile = args.out || 'report.json';
const only = args.only ? new Set(args.only.split(',')) : null;
const started = Date.now();
const goldenFile = (rel) => readJson(path.join(goldensDir(), rel + '.styles.json'));

const pw = loadPlaywright();
const browser = await pw.chromium.launch();
const report = {
  baseUrl,
  chromium: browser.version(),
  defaults: manifest.defaults || {},
  entries: [],
};

for (const entry of manifest.entries) {
  if (only && !only.has(entry.id)) continue;
  const rec = { id: entry.id, golden: entry.golden, status: 'pass', consoleErrors: [], comparisons: [] };
  report.entries.push(rec);
  if (entry.skip) {
    rec.status = 'skipped';
    rec.note = entry.skip;
    continue;
  }
  const golden = goldenFile(entry.golden);
  const res = await playEntry(browser, state, baseUrl, entry, async (page) => {
    for (const c of entry.comparisons || []) {
      // "from" borrows the element from another golden file (same state family).
      const g = c.from ? goldenFile(c.from) : golden;
      const cc = entry.offset && !c.offset ? { ...c, offset: entry.offset } : c;
      rec.comparisons.push(await runComparison(page, cc, g, manifest.defaults?.tol));
    }
  }, args['debug-dir']);
  rec.consoleErrors = res.consoleErrors;
  if (res.error) {
    rec.status = 'error';
    rec.error = res.error;
  } else if (rec.comparisons.some((c) => !c.pass) || rec.consoleErrors.length) {
    rec.status = 'fail';
  }
}
await browser.close();

// Table and detail lines.
let failed = 0;
let totalCmp = 0;
let failedCmp = 0;
console.log(`${'ENTRY'.padEnd(34)} ${'STATUS'.padEnd(8)} CMP  FAIL`);
for (const r of report.entries) {
  const nFail = r.comparisons.filter((c) => !c.pass).length;
  totalCmp += r.comparisons.length;
  failedCmp += nFail;
  if (r.status === 'fail' || r.status === 'error') failed++;
  console.log(`${r.id.padEnd(34)} ${r.status.padEnd(8)} ${String(r.comparisons.length).padStart(3)}  ${String(nFail).padStart(4)}`);
  if (r.error) console.log(`    error: ${r.error}`);
  for (const e of r.consoleErrors) console.log(`    console error: ${e.slice(0, 200)}`);
  for (const c of r.comparisons) {
    if (c.pass) continue;
    if (c.error) {
      console.log(`    ${r.id}/${c.id}: ${c.error}`);
      continue;
    }
    for (const p of c.props.filter((q) => !q.pass)) {
      console.log(`    ${r.id}/${c.id}.${p.prop}: expected ${p.expected} actual ${p.actual} (tol ${p.tol})`);
    }
  }
}
const nSkipped = report.entries.filter((r) => r.status === 'skipped').length;
console.log(`\n${report.entries.length} entries (${nSkipped} skipped), ${failed} failing; ${totalCmp} comparisons, ${failedCmp} failing; ${((Date.now() - started) / 1000).toFixed(1)}s`);
report.summary = { entries: report.entries.length, skipped: nSkipped, failingEntries: failed, comparisons: totalCmp, failingComparisons: failedCmp };
writeFileSync(outFile, JSON.stringify(report, null, 1) + '\n');
process.exit(failed ? 1 : 0);
