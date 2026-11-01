import { loadPlaywright, readJson, newContext, serverApi } from './lib.mjs';

const args = Object.fromEntries(
  process.argv.slice(2).reduce((a, v, i, all) => (v.startsWith('--') ? [...a, [v.slice(2), all[i + 1]]] : a), []),
);
if (!args.candidate) {
  console.error('usage: node finder.mjs --candidate STATE.json');
  process.exit(2);
}
const state = readJson(args.candidate);
const base = state.base;
const board = state.boards.gm;
const SEARCH_LIMIT_HITS = 25;
const SLOW_SEARCH_MS = 1500;
const ENTER_SETTLE_MS = 600;
const OPEN_WAIT_MS = 8000;
const FAILING_SEARCH_MS = 600;
const FAILURE_SETTLE_MS = 400;
const TYPED_OVER_SEARCH_MS = 1200;
const TYPED_OVER_WAIT_MS = 2800;
const QUEUED_SEARCH_MS = 1200;
const QUEUED_WAIT_MS = 2800;
const EMPTY_INPUT_QUERY = 'is:open';
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failures.push(what);
};

const top = async (q) => {
  const res = await serverApi(state, base, 'GET', `/api/search?limit=${SEARCH_LIMIT_HITS}&q=${encodeURIComponent(`board:${board} ${q}`)}`);
  return res.json?.items ?? [];
};
const pair = [];
for (const q of ['a', 'e', 'o', 't', 's', 'r', 'i', 'n']) {
  const items = await top(q);
  if (items.length && !pair.some((p) => p.items[0].task_id === items[0].task_id)) pair.push({ q, items });
  if (pair.length === 2) break;
}
if (pair.length < 2) {
  console.error('seed data gives no two queries with different top cards');
  process.exit(1);
}
const [first, second] = pair;

const pw = loadPlaywright();
const browser = await pw.chromium.launch();
const { ctx } = await newContext(browser, state, {}, base);
const page = await ctx.newPage();
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));

let delayMs = 0;
let failNext = false;
await page.route('**/api/search*', async (route) => {
  if (failNext) {
    failNext = false;
    await new Promise((r) => setTimeout(r, delayMs));
    return route.fulfill({ status: 400, contentType: 'application/json', body: '{"code":"invalid_request","message":"bad terms"}' });
  }
  if (delayMs) await new Promise((r) => setTimeout(r, delayMs));
  return route.continue();
});

const modalId = () => page.evaluate(() => document.querySelector('#modal-root .task-modal')?.dataset.taskId ?? null);
const closeModal = async () => {
  await page.evaluate(() => document.querySelector('[data-close-modal]')?.click());
  await page.waitForFunction(() => !document.querySelector('#modal-root .task-modal'));
};
const modalOpened = () => page.waitForFunction(() => document.querySelector('#modal-root .task-modal'), null, { timeout: OPEN_WAIT_MS }).catch(() => {});
const rows = () => page.$$eval('.finder-row', (r) => r.map((x) => x.classList.contains('finder-row--selected')));
const rowName = (i) => page.$eval(`.finder-row[data-finder-index="${i}"] .finder-row-name`, (e) => e.textContent);
const previewTitle = () => page.$eval('.finder-pv-title', (e) => e.textContent);
const openFinder = async () => {
  await page.keyboard.press('Control+k');
  await page.waitForSelector('.finder-row');
};
const retype = async (text) => {
  await page.fill('#finder-input', '');
  await page.keyboard.type(text);
};

await page.goto(`${base}/b/${board}`, { waitUntil: 'load' });
await page.waitForSelector('.task-card');
await openFinder();

await retype(first.q);
await page.waitForFunction((n) => document.querySelector('.finder-row-name')?.textContent === n, first.items[0].name);
await page.keyboard.press('ArrowDown');
await retype(second.q);
await page.waitForFunction((n) => document.querySelector('.finder-row-name')?.textContent === n, second.items[0].name);
await page.waitForFunction((n) => document.querySelector('.finder-pv-title')?.textContent === n, second.items[0].name);
const sel = await rows();
check(sel[0] === true && sel.filter(Boolean).length === 1, 'a second result list selects its first row');
check((await previewTitle()) === (await rowName(0)), 'the preview follows the first row of the second list');

delayMs = SLOW_SEARCH_MS;
await retype(first.q);
await page.keyboard.press('Enter');
await page.waitForTimeout(ENTER_SETTLE_MS);
check((await modalId()) === null, 'Enter during a delayed search opens nothing at once');
await modalOpened();
check((await modalId()) === first.items[0].task_id, 'Enter during a delayed search opens the first card of the new list');
await closeModal();

delayMs = 0;
await openFinder();
await retype(second.q);
await page.waitForFunction((n) => document.querySelector('.finder-row-name')?.textContent === n, second.items[0].name);
delayMs = FAILING_SEARCH_MS;
failNext = true;
await retype('"unterminated');
await page.keyboard.press('Enter');
await page.waitForFunction(() => [...document.querySelectorAll('.finder-note')].some((n) => n.textContent === 'bad terms'));
await page.waitForTimeout(FAILURE_SETTLE_MS);
check((await modalId()) === null, 'Enter during a failed search opens nothing');
check(await page.$eval('#finder', (e) => !e.hidden), 'the finder stays open after a failed search');

delayMs = TYPED_OVER_SEARCH_MS;
await retype(second.q);
await page.keyboard.press('Enter');
await page.keyboard.type('x');
await page.waitForTimeout(TYPED_OVER_WAIT_MS);
check((await modalId()) === null, 'typing after Enter drops the queued Enter');
check(await page.$eval('#finder', (e) => !e.hidden), 'the finder stays open after typing over a queued Enter');
delayMs = 0;

const queueEnterOverChip = async () => {
  await page.keyboard.press('Escape');
  await page.evaluate(() => window.localStorage.removeItem('pomb-finder-recent'));
  delayMs = 0;
  await openFinder();
  delayMs = QUEUED_SEARCH_MS;
  await page.keyboard.type('a');
  await page.keyboard.press('Backspace');
  await page.keyboard.press('Enter');
};

await queueEnterOverChip();
await page.waitForTimeout(ENTER_SETTLE_MS);
check((await modalId()) === null, 'Enter over the board chip opens nothing while the search is out');
await modalOpened();
check((await modalId()) === (await top(EMPTY_INPUT_QUERY))[0]?.task_id, 'Enter over the board chip is queued and opens the top card when the search lands');
await closeModal();

await queueEnterOverChip();
await page.keyboard.press('Backspace');
await page.waitForTimeout(QUEUED_WAIT_MS);
check((await modalId()) === null, 'removing the board chip drops the queued Enter');
check(await page.$eval('#finder', (e) => !e.hidden), 'the finder stays open after removing the chip over a queued Enter');
delayMs = 0;

const RECENT_KEY = 'pomb-finder-recent';
const GONE_NAME = 'Finder stale recent probe';
const columns = await serverApi(state, base, 'GET', `/api/boards/${board}/columns`);
const gone = await serverApi(state, base, 'POST', '/api/tasks', { column_id: columns.json[0].id, name: GONE_NAME });
const goneId = gone.json?.id;
check(Boolean(goneId), 'a probe card is created');
const live = first.items[0];
const entryFor = (item, name) => ({
  task_id: item.task_id ?? item.id,
  name,
  board_id: board,
  board_name: item.board_name ?? 'Board',
  column_id: item.column_id ?? columns.json[0].id,
  column_name: item.column_name ?? columns.json[0].name,
  status: 'open',
  matched_field: 'title',
  matched_index: null,
  ranges: [],
});
const seedRecent = (entries) => page.evaluate(([key, list]) => window.localStorage.setItem(key, JSON.stringify(list)), [RECENT_KEY, entries]);
const storedIds = () => page.evaluate((key) => JSON.parse(window.localStorage.getItem(key) || '[]').map((e) => e.task_id), RECENT_KEY);
const rowNames = () => page.$$eval('.finder-row-name', (r) => r.map((x) => x.textContent));
const toastText = () => page.$eval('#toast', (e) => (e.hidden ? '' : e.textContent));

await page.keyboard.press('Escape');
await seedRecent([entryFor(gone.json, GONE_NAME), entryFor(live, live.name)]);
await serverApi(state, base, 'DELETE', `/api/tasks/${goneId}`);
await openFinder();
check((await rowNames())[0] === GONE_NAME, 'the recent list shows the deleted card before it is activated');
await page.keyboard.press('Enter');
await page.waitForFunction(() => document.querySelector('#toast') && !document.querySelector('#toast').hidden);
check((await toastText()).includes('no longer exists'), 'activating a recent entry for a deleted card shows a note');
check(!(await rowNames()).includes(GONE_NAME), 'activating a recent entry for a deleted card removes the row');
check(!(await storedIds()).includes(goneId), 'the deleted card leaves the stored recent list');
check((await modalId()) === null, 'a deleted recent card opens no modal');
check(await page.$eval('#finder', (e) => !e.hidden), 'the finder stays open after dropping a stale recent entry');
await page.keyboard.press('Enter');
await modalOpened();
check((await modalId()) === live.task_id, 'a recent entry for a live card still opens');
await closeModal();

check(errors.length === 0, `no page errors${errors.length ? `: ${errors[0]}` : ''}`);
await browser.close();
process.exit(failures.length ? 1 : 0);
