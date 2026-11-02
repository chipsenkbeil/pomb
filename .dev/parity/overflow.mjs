import { loadPlaywright, readJson, newContext } from './lib.mjs';

const stateFile = process.argv[2];
const width = Number(process.argv[3]);
const CLICK_TIMEOUT_MS = 5000;
const SETTLE_MS = 300;
const MAX_OFFENDERS = 12;
const state = readJson(stateFile);
const pw = loadPlaywright();
const browser = await pw.chromium.launch();
const gm = state.boards.gm;
const vt = state.boards.vt;
const pages = [
  { name: 'board-gm', url: `/b/${gm}` },
  { name: 'board-vt', url: `/b/${vt}` },
  { name: 'board-gm-modal', url: `/b/${gm}`, modal: true },
  { name: 'board-vt-modal', url: `/b/${vt}`, modal: true },
  { name: 'settings', url: '/settings' },
  { name: 'timer-log', url: '/timer/log' },
  { name: 'timer-statistics', url: '/timer/statistics' },
  { name: 'task-count', url: `/b/${gm}/task-count` },
  { name: 'throughput', url: `/b/${gm}/throughput` },
  { name: 'board-history', url: `/b/${gm}/board-history` },
  { name: 'export', url: `/b/${gm}/export` },
];

function probe(w) {
  const root = document.documentElement;
  const clipped = (el) => {
    for (let p = el.parentElement; p && p !== root && p !== document.body; p = p.parentElement) {
      const o = getComputedStyle(p);
      if (o.overflowX !== 'visible') return true;
    }
    return false;
  };
  const offenders = [];
  for (const el of document.querySelectorAll('body, body *')) {
    const r = el.getBoundingClientRect();
    if (r.width === 0 || r.right <= w + 0.5) continue;
    if (clipped(el)) continue;
    const cs = getComputedStyle(el);
    if (cs.position === 'fixed') continue;
    const id = el.tagName.toLowerCase() + (el.id ? '#' + el.id : '') + (el.className && typeof el.className === 'string' ? '.' + el.className.trim().split(/\s+/).join('.') : '');
    offenders.push(`${id} right=${Math.round(r.right)} w=${Math.round(r.width)}`);
  }
  return { sw: root.scrollWidth, bodySw: document.body.scrollWidth, offenders: offenders };
}

let bad = 0;
for (const p of pages) {
  const { ctx } = await newContext(browser, state, { viewport: { width, height: 844 } }, state.base);
  const page = await ctx.newPage();
  try {
    const res = await page.goto(state.base + p.url, { waitUntil: 'load' });
    if (res.status() >= 400) { console.log(`${p.name} HTTP ${res.status()}`); bad++; await ctx.close(); continue; }
    if (p.modal) {
      await page.locator('.task-card .card-title').first().click({ timeout: CLICK_TIMEOUT_MS });
      await page.locator('.task-modal').first().waitFor({ state: 'visible', timeout: CLICK_TIMEOUT_MS });
    }
    await page.waitForTimeout(SETTLE_MS);
    const r = await page.evaluate(probe, width);
    const ok = r.sw <= width;
    if (!ok) bad++;
    console.log(`${p.name} ${ok ? 'ok' : 'OVERFLOW'} scrollWidth=${r.sw} body=${r.bodySw}`);
    if (!ok) for (const o of r.offenders.slice(0, MAX_OFFENDERS)) console.log('   ' + o);
  } catch (e) {
    bad++;
    console.log(`${p.name} error ${String(e).split('\n')[0]}`);
  }
  await ctx.close();
}
await browser.close();
process.exit(bad ? 1 : 0);
