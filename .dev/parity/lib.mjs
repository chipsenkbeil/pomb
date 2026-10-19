// Shared helpers for run.mjs and regress.mjs: Playwright loading, state,
// deterministic browser contexts, and manifest step replay.
import { createRequire } from 'node:module';
import { readFileSync, mkdirSync, existsSync, writeFileSync, renameSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';

process.env.PLAYWRIGHT_BROWSERS_PATH ??= '0';

export function loadPlaywright() {
  const nm = process.env.PW_NODE_MODULES;
  if (!nm) throw new Error('set PW_NODE_MODULES to the node_modules dir holding playwright 1.63');
  return createRequire(path.join(path.resolve(nm), 'x.js'))('playwright');
}

export const goldensDir = () =>
  process.env.CHIPFLOW_GOLDENS || path.join(homedir(), 'Pictures', 'chipflow-goldens');

export function readJson(p) {
  return JSON.parse(readFileSync(p, 'utf8'));
}

// Netscape cookie jar (curl -c, #HttpOnly_ lines included) to Playwright cookies.
export function cookiesFromJar(jar) {
  const out = [];
  for (const raw of readFileSync(jar, 'utf8').split('\n')) {
    let line = raw.trim();
    let httpOnly = false;
    if (line.startsWith('#HttpOnly_')) {
      httpOnly = true;
      line = line.slice(10);
    } else if (!line || line.startsWith('#')) continue;
    const [domain, , p, secure, , name, value = ''] = line.split('\t');
    if (name) out.push({ name, value, domain, path: p, httpOnly, secure: secure === 'TRUE' });
  }
  return out;
}

// Epoch ms to server-local wall clock "YYYY-MM-DD HH:MM:SS" in tz.
export function msToWall(ms, tz) {
  const fmt = new Intl.DateTimeFormat('en-CA', {
    timeZone: tz, year: 'numeric', month: '2-digit', day: '2-digit',
    hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23',
  });
  const p = Object.fromEntries(fmt.formatToParts(new Date(ms)).map((x) => [x.type, x.value]));
  return `${p.year}-${p.month}-${p.day} ${p.hour}:${p.minute}:${p.second}`;
}

// Point the frozen server clock at ms. libfaketime re-reads the file at most
// about once a second, so wait that out before the server is used again. The
// rename replaces the file whole: a half-written file fails to parse and
// libfaketime kills the server.
export async function setServerClock(state, ms) {
  writeFileSync(state.ftfile + '.tmp', msToWall(ms, state.tz));
  renameSync(state.ftfile + '.tmp', state.ftfile);
  await new Promise((r) => setTimeout(r, 1100));
}
// Node-side API call with the session cookie (no browser needed).
export async function serverApi(state, baseUrl, method, p, body) {
  const cookie = cookiesFromJar(state.jar).map((c) => `${c.name}=${c.value}`).join('; ');
  const res = await fetch(baseUrl + p, {
    method,
    headers: { cookie, origin: baseUrl, accept: 'application/json', 'content-type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  try { return { status: res.status, json: JSON.parse(text) }; } catch { return { status: res.status, text }; }
}

// Undo what a timer entry changes on the server: timer stopped and today's
// time entries deleted (while the clock is still where the entry left it, so
// the stop sees a running session), then the clock back to the base time.
export async function resetServer(state, baseUrl) {
  await serverApi(state, baseUrl, 'POST', '/api/timer/stop', {});
  const today = state.faketime.split(' ')[0];
  const log = await serverApi(state, baseUrl, 'GET', '/api/timer/log');
  for (const e of log.json?.entries || []) {
    if (e.day_key >= today) await serverApi(state, baseUrl, 'DELETE', `/api/time/entries/${e.id}`);
  }
  await setServerClock(state, wallToMs(state.faketime, state.tz));
}

// Server-local wall clock "YYYY-MM-DD HH:MM:SS" in tz to epoch ms.
export function wallToMs(wall, tz) {
  const [d, t] = wall.split(' ');
  const guess = Date.parse(`${d}T${t}Z`);
  const fmt = new Intl.DateTimeFormat('en-CA', {
    timeZone: tz, year: 'numeric', month: '2-digit', day: '2-digit',
    hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23',
  });
  const p = Object.fromEntries(fmt.formatToParts(new Date(guess)).map((x) => [x.type, x.value]));
  const shown = Date.parse(`${p.year}-${p.month}-${p.day}T${p.hour}:${p.minute}:${p.second}Z`);
  return guess - (shown - guess);
}

const FREEZE_CSS =
  '*,*::before,*::after{transition:none!important;animation:none!important;' +
  'caret-color:transparent!important;scroll-behavior:auto!important}';

// htmx and SortableJS come from a CDN; cache them next to the state file so
// replays do not depend on the network.
async function cdnRoute(ctx, cacheDir) {
  mkdirSync(cacheDir, { recursive: true });
  await ctx.route(/^https:\/\/cdn\.jsdelivr\.net\//, async (route) => {
    const url = route.request().url();
    const file = path.join(cacheDir, url.replace(/[^A-Za-z0-9.]+/g, '_'));
    if (!existsSync(file)) {
      const res = await fetch(url);
      if (!res.ok) return route.abort();
      writeFileSync(file, Buffer.from(await res.arrayBuffer()));
    }
    return route.fulfill({ path: file, contentType: 'application/javascript' });
  });
}

// A fresh, fully pinned browser context for one manifest entry.
export async function newContext(browser, state, entry, baseUrl) {
  const vp = entry.viewport || { width: 1440, height: 612 };
  const ctx = await browser.newContext({
    viewport: vp,
    deviceScaleFactor: 1,
    reducedMotion: 'reduce',
    colorScheme: 'light',
    locale: 'en-US',
    timezoneId: state.tz,
  });
  const url = new URL(baseUrl);
  const jarCookies = cookiesFromJar(state.jar).map((c) => ({ ...c, domain: url.hostname }));
  await ctx.addCookies(jarCookies);
  await ctx.addInitScript((css) => {
    const inject = () => {
      const s = document.createElement('style');
      s.textContent = css;
      document.documentElement.appendChild(s);
    };
    if (document.documentElement) inject();
    else document.addEventListener('DOMContentLoaded', inject);
  }, FREEZE_CSS);
  const nowMs = wallToMs(state.faketime, state.tz);
  if (entry.clock === 'controlled') {
    await ctx.clock.install({ time: nowMs });
    await ctx.clock.pauseAt(nowMs + 1);
  } else {
    await ctx.clock.setFixedTime(nowMs);
  }
  await cdnRoute(ctx, path.join(path.dirname(state.jar), 'cdn-cache'));
  return { ctx, nowMs, info: { clockMs: nowMs + 1 } };
}

const sub = (s, state) =>
  typeof s === 'string'
    ? s.replaceAll('{gm}', state.boards.gm).replaceAll('{vt}', state.boards.vt)
    : s;

// In-page fetch with the session cookie; used by "api" steps.
async function pageApi(page, method, p, body) {
  return page.evaluate(
    async ([m, u, b]) => {
      const r = await fetch(u, {
        method: m,
        headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
        credentials: 'same-origin',
        body: b ? JSON.stringify(b) : undefined,
      });
      const t = await r.text();
      try { return { status: r.status, json: JSON.parse(t) }; } catch { return { status: r.status, text: t.slice(0, 200) }; }
    },
    [method, p, body],
  );
}

async function resolveTasks(page, state, text) {
  const tokens = [...new Set(text.match(/\{task:(gm|vt):[^}]+\}/g) || [])];
  for (const tok of tokens) {
    const [, which, prefix] = /^\{task:(gm|vt):(.+)\}$/.exec(tok);
    const tasks = await pageApi(page, 'GET', `/api/boards/${state.boards[which]}/tasks`);
    const task = tasks.json.items.find((t) => t.name.startsWith(prefix));
    if (!task) throw new Error(`no task ${prefix}`);
    text = text.replaceAll(tok, task.id);
  }
  return text;
}

// Replay a setup step list. Step kinds are documented in README.md.
export async function runSteps(page, steps, state, baseUrl, ctxInfo) {
  for (const raw of steps || []) {
    const step = JSON.parse(JSON.stringify(raw), (_, v) => sub(v, state));
    const [kind] = Object.keys(step);
    const arg = step[kind];
    switch (kind) {
      case 'goto':
        await page.goto(baseUrl + arg, { waitUntil: 'load' });
        break;
      case 'click':
        await page.locator(arg).nth(step.nth || 0).click({ button: step.button || 'left', timeout: 5000 });
        break;
      case 'dblclick':
        await page.locator(arg).nth(step.nth || 0).dblclick({ timeout: 5000 });
        break;
      case 'hover':
        await page.locator(arg).nth(step.nth || 0).hover({ timeout: 5000 });
        break;
      case 'contextmenu':
        await page.locator(arg).nth(step.nth || 0).dispatchEvent('contextmenu', { bubbles: true, cancelable: true, clientX: step.x ?? 0, clientY: step.y ?? 0, button: 2 });
        break;
      case 'fill':
        await page.locator(arg).nth(step.nth || 0).fill(step.text);
        break;
      case 'select':
        await page.locator(arg).nth(step.nth || 0).selectOption(step.value);
        break;
      case 'press':
        await page.keyboard.press(arg);
        break;
      case 'mouse':
        await page.mouse.move(arg[0], arg[1]);
        break;
      case 'clickAt':
        await page.mouse.click(arg[0], arg[1], { button: step.button || 'left' });
        break;
      case 'dragTo': {
        // Press at arg, move in steps to step.to, leave the button down.
        await page.mouse.move(arg[0], arg[1]);
        await page.mouse.down();
        const n = step.steps || 8;
        for (let i = 1; i <= n; i++) {
          await page.mouse.move(arg[0] + ((step.to[0] - arg[0]) * i) / n, arg[1] + ((step.to[1] - arg[1]) * i) / n);
          await page.waitForTimeout(30);
        }
        break;
      }
      case 'waitFor':
        await page.locator(arg).first().waitFor({ state: step.state || 'visible', timeout: 5000 });
        break;
      case 'wait':
        await page.waitForTimeout(arg);
        break;
      case 'eval':
        await page.evaluate(arg);
        break;
      case 'api': {
        const [m, p, b] = arg;
        // "{task:vt:Task Beta}" inside the body becomes that task's id.
        const body = b ? JSON.parse(await resolveTasks(page, state, JSON.stringify(b))) : b;
        const r = await pageApi(page, m, p, body);
        if (r.status >= 400) throw new Error(`api ${m} ${p} -> ${r.status} ${JSON.stringify(r.json || r.text)}`);
        break;
      }
      case 'apiColumn': {
        // ["gm"|"vt", "Column name", {patch}] on the seeded board's column.
        const [which, name, body] = arg;
        const cols = await pageApi(page, 'GET', `/api/boards/${state.boards[which]}/columns`);
        const col = cols.json.find((c) => c.name === name);
        if (!col) throw new Error(`no column ${name}`);
        const r = await pageApi(page, 'PATCH', `/api/columns/${col.id}`, body);
        if (r.status >= 400) throw new Error(`PATCH column ${name} -> ${r.status}`);
        break;
      }
      case 'apiTask': {
        // ["gm"|"vt", "name prefix", METHOD, "/suffix", {body}] on /api/tasks/{id}.
        const [which, prefix, method, suffix, body] = arg;
        const tasks = await pageApi(page, 'GET', `/api/boards/${state.boards[which]}/tasks`);
        const task = tasks.json.items.find((t) => t.name.startsWith(prefix));
        if (!task) throw new Error(`no task ${prefix}`);
        const r = await pageApi(page, method, `/api/tasks/${task.id}${suffix || ''}`, body);
        if (r.status >= 400) throw new Error(`${method} task ${prefix}${suffix} -> ${r.status}`);
        break;
      }
      case 'reload':
        await page.reload({ waitUntil: 'load' });
        break;
      case 'advance':
        // Controlled clock only: move server and browser clocks forward by
        // arg seconds. The last 5 seconds run in one-second steps, each
        // followed by a real pause so the page's status poll lands before
        // the next step; the rest is one jump.
        {
          const slow = Math.min(arg, 5);
          // Let in-flight status polls land before the clocks move.
          await page.waitForTimeout(300);
          if (arg > slow) {
            ctxInfo.clockMs += (arg - slow) * 1000;
            await setServerClock(state, ctxInfo.clockMs);
            await page.clock.fastForward((arg - slow) * 1000);
          }
          for (let i = 0; i < slow; i++) {
            ctxInfo.clockMs += 1000;
            await setServerClock(state, ctxInfo.clockMs);
            await page.clock.runFor(1000);
            await page.waitForTimeout(150);
          }
          // Resync the page with the server clock (its poll interval is long).
          await page.evaluate(() => window.TimerUI && window.TimerUI.refresh());
          await page.waitForTimeout(300);
        }
        break;
      case 'jump':
        // Controlled clock only: move both clocks by arg seconds at once.
        ctxInfo.clockMs += arg * 1000;
        await setServerClock(state, ctxInfo.clockMs);
        await page.clock.fastForward(arg * 1000);
        break;
      case 'resize':
        await page.setViewportSize({ width: arg[0], height: arg[1] });
        break;
      case 'localStorage':
        await page.evaluate(([k, v]) => localStorage.setItem(k, v), arg);
        break;
      default:
        throw new Error(`unknown step ${kind}`);
    }
  }
}

// Wait for fonts, network idle and a fixed settle delay.
export async function settle(page, ms = 400) {
  await page.evaluate(() => document.fonts.ready);
  await page.waitForLoadState('networkidle').catch(() => {});
  await page.waitForTimeout(ms);
}

// manifest.json holds defaults and an "include" list of files whose "entries"
// arrays are concatenated in order.
export function loadManifest(file) {
  const m = readJson(file);
  const entries = [...(m.entries || [])];
  for (const inc of m.include || []) {
    entries.push(...readJson(path.join(path.dirname(file), inc)).entries);
  }
  return { ...m, entries };
}

// Replay one entry: reset the server for controlled-clock entries, open a
// fresh context, run the setup, settle, call inspect(page), run the teardown
// and clean up. Returns { consoleErrors, error } and never throws, so one
// broken entry does not stop the run. debugDir keeps a screenshot where the
// setup or inspect step broke.
export async function playEntry(browser, state, baseUrl, entry, inspect, debugDir = null) {
  const out = { consoleErrors: [], error: null };
  let ctx;
  let page;
  try {
    if (entry.clock === 'controlled') await resetServer(state, baseUrl);
    let info;
    ({ ctx, info } = await newContext(browser, state, entry, baseUrl));
    page = await ctx.newPage();
    page.on('console', (m) => m.type() === 'error' && out.consoleErrors.push(m.text()));
    page.on('pageerror', (e) => out.consoleErrors.push(String(e)));
    await runSteps(page, entry.setup, state, baseUrl, info);
    await settle(page, entry.settleMs ?? 500);
    // Two animation frames, so layout and paint have caught up (a paused
    // browser clock never fires them).
    if (entry.clock !== 'controlled') {
      await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
    }
    // Jump running CSS animations to their end state, so a screenshot never
    // catches one mid-way.
    await page.evaluate(() => document.getAnimations().forEach((a) => a.finish()));
    await inspect(page);
  } catch (e) {
    if (debugDir && page) await page.screenshot({ path: path.join(debugDir, `${entry.id}.png`) }).catch(() => {});
    out.error = String(e.message || e).split('\n').slice(0, 6).join(' ').slice(0, 600);
  } finally {
    // "teardown" undoes server-side changes the setup made, so entries stay
    // independent and a second run starts from the same data.
    if (page && entry.teardown) {
      try {
        await page.goto(baseUrl + '/login', { waitUntil: 'load' });
        await runSteps(page, entry.teardown, state, baseUrl);
      } catch (e) {
        out.error ??= `teardown: ${String(e.message || e).split('\n')[0]}`;
      }
    }
    if (ctx) await ctx.close();
    if (entry.clock === 'controlled') await resetServer(state, baseUrl);
  }
  return out;
}

// Return a predicate for `--only id,id`. Each id selects the entry with that
// id or whose id starts with it plus "-" (board-01 selects
// board-01-test-board). Exits 2 when an id selects nothing.
export function onlyFilter(onlyArg, entries) {
  if (!onlyArg) return () => true;
  const wanted = onlyArg.split(',');
  const matches = (id, want) => id === want || id.startsWith(want + '-');
  const unmatched = wanted.filter((want) => !entries.some((entry) => matches(entry.id, want)));
  if (unmatched.length) {
    console.error(`--only matches no manifest entry: ${unmatched.join(', ')}`);
    process.exit(2);
  }
  return (id) => wanted.some((want) => matches(id, want));
}
