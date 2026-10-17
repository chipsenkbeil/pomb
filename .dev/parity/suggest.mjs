// Authoring aid: replay a manifest entry's setup, then for each golden
// element print the ChipFlow element whose box overlaps it most, with a CSS
// selector and index. Use it to write comparisons for a new entry.
//
//   node suggest.mjs --state STATE.json --entry <id> [--manifest manifest.json]
//
// Not a gate: the output is a starting point, check each pairing by eye.
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadPlaywright, newContext, runSteps, settle, readJson, goldensDir, loadManifest } from './lib.mjs';

const args = Object.fromEntries(process.argv.slice(2).reduce((a, v, i, all) => (v.startsWith('--') ? [...a, [v.slice(2), all[i + 1]]] : a), []));
const here = path.dirname(fileURLToPath(import.meta.url));
const state = readJson(args.state);
const manifest = loadManifest(args.manifest || path.join(here, 'manifest.json'));
const entry = manifest.entries.find((e) => e.id === args.entry);
if (!entry) throw new Error(`no entry ${args.entry}`);
const golden = readJson(path.join(goldensDir(), entry.golden + '.styles.json'));

const pw = loadPlaywright();
const browser = await pw.chromium.launch();
const { ctx, info } = await newContext(browser, state, entry, state.base);
const page = await ctx.newPage();
await runSteps(page, entry.setup, state, state.base, info);
await settle(page);

const els = golden.els.map((e) => {
  const n = (e[1] || '').split(',').map(Number);
  return { label: e[0], box: n.length === 4 && n.every((v) => !Number.isNaN(v)) ? n : null, style: e[2] };
});

const result = await page.evaluate((els) => {
  const cssPath = (el) => {
    if (el.id) return '#' + CSS.escape(el.id);
    const cls = [...el.classList].filter((c) => !/^(taskColor|taskBorderColor|taskColorVars)-/.test(c)).map((c) => '.' + CSS.escape(c)).join('');
    return el.tagName.toLowerCase() + cls;
  };
  const pool = [...document.querySelectorAll('body *')].map((el) => ({ el, r: el.getBoundingClientRect() })).filter((p) => p.r.width > 0 && p.r.height > 0);
  return els.map((g) => {
    if (!g.box) return { label: g.label, none: 'no box' };
    const [gx, gy, gw, gh] = g.box;
    let best = null;
    for (const p of pool) {
      const ix = Math.max(0, Math.min(gx + gw, p.r.right) - Math.max(gx, p.r.left));
      const iy = Math.max(0, Math.min(gy + gh, p.r.bottom) - Math.max(gy, p.r.top));
      const inter = ix * iy;
      const iou = inter / (gw * gh + p.r.width * p.r.height - inter || 1);
      if (!best || iou > best.iou) best = { p, iou };
    }
    if (!best || best.iou < 0.2) return { label: g.label, none: 'no overlap' };
    const sel = cssPath(best.p.el);
    const all = [...document.querySelectorAll(sel)];
    return {
      label: g.label, iou: +best.iou.toFixed(2), sel, nth: all.indexOf(best.p.el),
      dom: [best.p.r.left, best.p.r.top, best.p.r.width, best.p.r.height].map((v) => +v.toFixed(1)),
    };
  });
}, els);

result.forEach((r, i) => console.log(JSON.stringify({ ...r, golden: els[i].box })));
await browser.close();
