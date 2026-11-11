var src = require('fs').readFileSync(process.argv[2], 'utf8');
var from = src.indexOf('  var BoardHistoryPage = (function () {');
var to = src.indexOf('  // Calendar report: tasks laid out by due date', from);
if (from < 0 || to < 0) throw new Error('BoardHistoryPage not found');
var BoardHistoryPage;
function escapeHtml(s) { return String(s); }
var document = {};
eval(src.slice(from, to));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function day(d) { return d && [d.getFullYear(), d.getMonth() + 1, d.getDate()].join('-'); }
function range(period, now, f, t) {
  var r = BoardHistoryPage.periodRange(period, now, f, t);
  return [day(r.from), day(r.to)];
}

var wednesday = new Date(2026, 9, 28, 15, 30);
check('latest is open', range('latest', wednesday), [null, null]);
check('this week runs Monday to the next Monday', range('this-week', wednesday), ['2026-10-26', '2026-11-2']);
check('last week is the seven days before', range('last-week', wednesday), ['2026-10-19', '2026-10-26']);
check('this month runs to the first of the next', range('this-month', wednesday), ['2026-10-1', '2026-11-1']);
check('last month wraps the year', range('last-month', new Date(2026, 0, 15)), ['2025-12-1', '2026-1-1']);
check('custom includes the last day', range('custom', wednesday, '2026-10-01', '2026-10-31'), ['2026-10-1', '2026-11-1']);
check('custom with one end is open at the other', range('custom', wednesday, '', '2026-10-31'), [null, '2026-11-1']);

var query = BoardHistoryPage.queryString({
  event: 'changed', property: 'moved', user: 'ann b', period: 'latest', from: '', to: '', order: 'oldest',
}, wednesday);
check('query carries the filters', query, 'event=changed&property=moved&user=ann%20b&order=oldest');
check('property is dropped outside the changed event', BoardHistoryPage.queryString({
  event: 'comment', property: 'moved', user: '', period: 'latest', from: '', to: '', order: 'newest',
}, wednesday), 'event=comment&order=newest');
var dated = BoardHistoryPage.queryString({
  event: '', property: '', user: '', period: 'last-week', from: '', to: '', order: '',
}, wednesday);
check('period sends both instants', /^from=[^&]+&to=[^&]+$/.test(dated), true);

const old = JSON.stringify([
  { name: ' Ann moves ', filters: { 'history-event-filter': 'changed', 'history-user-filter': 'ann', stray: 'x', 'history-from': 3 } },
  { name: '   ', filters: {} },
  { name: 'No filters' },
  { filters: { 'history-order-filter': 'oldest' } },
]);
check('history_bookmark_import_should_post_old_entries', BoardHistoryPage.legacyBookmarkBodies(old), [
  { report: 'history', name: 'Ann moves', filters: { 'history-event-filter': 'changed', 'history-user-filter': 'ann' } },
  { report: 'history', name: 'No filters', filters: {} },
]);
check('a broken old key imports nothing', BoardHistoryPage.legacyBookmarkBodies('{oops'), []);
check('an old key that is not a list imports nothing', BoardHistoryPage.legacyBookmarkBodies('{"a":1}'), []);

const HISTORY_KEY = 'pomb-history-bookmarks-b1';
const legacyList = (names) => JSON.stringify(names.map((name) => ({ name, filters: {} })));
const storageWith = (raw) => {
  const map = { [HISTORY_KEY]: raw };
  const writes = [];
  return {
    writes,
    getItem: (key) => (key in map ? map[key] : null),
    setItem: (key, value) => { writes.push(JSON.parse(value).map((b) => b.name)); map[key] = value; },
    removeItem: (key) => { delete map[key]; },
  };
};
const replier = (replies) => {
  const sent = [];
  const post = (body) => {
    sent.push(body.name);
    const reply = replies.shift();
    return reply ? Promise.reject(reply) : Promise.resolve();
  };
  return { sent, post };
};
const toaster = () => {
  const seen = [];
  return { seen, toast: (message) => seen.push(message) };
};
const names = (raw) => JSON.parse(raw).map((b) => b.name);

const cases = [];
const testCase = (fn) => cases.push(fn);

testCase(async () => {
  const storage = storageWith(legacyList(['A', 'B', 'C']));
  const { sent, post } = replier([{ status: 409 }, undefined, { status: 400 }]);
  const { seen, toast } = toaster();
  await BoardHistoryPage.importLegacyBookmarks(HISTORY_KEY, post, storage, toast);
  check('history_bookmark_import_should_drop_refused_entries_and_toast_once posts every entry', sent, ['A', 'B', 'C']);
  check('history_bookmark_import_should_drop_refused_entries_and_toast_once clears the refused entries', storage.getItem(HISTORY_KEY), null);
  check('history_bookmark_import_should_drop_refused_entries_and_toast_once toasts once with the count', seen, ['Dropped 2 old bookmarks the board refused.']);
});

testCase(async () => {
  const storage = storageWith(legacyList(['A', 'B', 'C']));
  const { sent, post } = replier([undefined, new TypeError('Failed to fetch')]);
  const { seen, toast } = toaster();
  await BoardHistoryPage.importLegacyBookmarks(HISTORY_KEY, post, storage, toast);
  check('history_bookmark_import_should_keep_entries_after_a_network_failure stops at the failed entry', sent, ['A', 'B']);
  check('history_bookmark_import_should_keep_entries_after_a_network_failure keeps the failed and later entries', names(storage.getItem(HISTORY_KEY)), ['B', 'C']);
  check('history_bookmark_import_should_keep_entries_after_a_network_failure shows no toast', seen, []);
});

testCase(async () => {
  const storage = storageWith(legacyList(['A', 'B']));
  const { sent, post } = replier([{ status: 500 }]);
  const { seen, toast } = toaster();
  await BoardHistoryPage.importLegacyBookmarks(HISTORY_KEY, post, storage, toast);
  check('a 5xx keeps the failed entry for the next load', names(storage.getItem(HISTORY_KEY)), ['A', 'B']);
  check('a 5xx shows no toast when nothing was refused', seen, []);
  check('a 5xx stops the import', sent, ['A']);
});

testCase(async () => {
  const storage = storageWith(legacyList(['A', 'B', 'C']));
  const { sent, post } = replier([]);
  const { seen, toast } = toaster();
  await BoardHistoryPage.importLegacyBookmarks(HISTORY_KEY, post, storage, toast);
  check('history_bookmark_import_should_post_in_order_and_clear_the_key_with_no_toast posts in order', sent, ['A', 'B', 'C']);
  check('history_bookmark_import_should_post_in_order_and_clear_the_key_with_no_toast shrinks the key after each post', storage.writes, [['B', 'C'], ['C']]);
  check('history_bookmark_import_should_post_in_order_and_clear_the_key_with_no_toast clears the key', storage.getItem(HISTORY_KEY), null);
  check('history_bookmark_import_should_post_in_order_and_clear_the_key_with_no_toast shows no toast', seen, []);
});

testCase(async () => {
  for (const status of [401, 403, 408, 429]) {
    const storage = storageWith(legacyList(['A', 'B']));
    const { sent, post } = replier([{ status }]);
    const { seen, toast } = toaster();
    await BoardHistoryPage.importLegacyBookmarks(HISTORY_KEY, post, storage, toast);
    check('history_bookmark_import_should_keep_entries_on_session_and_rate_limit_replies keeps the entry after ' + status, names(storage.getItem(HISTORY_KEY)), ['A', 'B']);
    check('history_bookmark_import_should_keep_entries_on_session_and_rate_limit_replies stops after ' + status, sent, ['A']);
    check('history_bookmark_import_should_keep_entries_on_session_and_rate_limit_replies shows no toast after ' + status, seen, []);
  }
});

const fakeLocks = () => {
  const held = [];
  let tail = Promise.resolve();
  return {
    held,
    request: (name, fn) => {
      held.push(name);
      const run = tail.then(fn);
      tail = run.catch(() => {});
      return run;
    },
  };
};
const sharedStorage = (raw) => {
  const map = { [HISTORY_KEY]: raw };
  return {
    getItem: (key) => (key in map ? map[key] : null),
    setItem: (key, value) => { map[key] = value; },
    removeItem: (key) => { delete map[key]; },
  };
};
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

testCase(async () => {
  const storage = sharedStorage(legacyList(['A', 'B', 'C']));
  const locks = fakeLocks();
  const sent = [];
  const post = (body) => tick().then(() => { sent.push(body.name); });
  const { toast } = toaster();
  await Promise.all([
    BoardHistoryPage.importLegacyBookmarks(HISTORY_KEY, post, storage, toast, () => [], locks),
    BoardHistoryPage.importLegacyBookmarks(HISTORY_KEY, post, storage, toast, () => [], locks),
  ]);
  check('history_bookmark_import_should_post_each_entry_once_across_tabs posts each entry once', sent, ['A', 'B', 'C']);
  check('history_bookmark_import_should_post_each_entry_once_across_tabs locks the per-board name', locks.held, [HISTORY_KEY, HISTORY_KEY]);
  check('history_bookmark_import_should_post_each_entry_once_across_tabs clears the key', storage.getItem(HISTORY_KEY), null);
});

testCase(async () => {
  const storage = storageWith(legacyList(['A', 'B', 'C']));
  const { sent, post } = replier([]);
  const { seen, toast } = toaster();
  await BoardHistoryPage.importLegacyBookmarks(HISTORY_KEY, post, storage, toast, () => ['A', 'C']);
  check('history_bookmark_import_should_skip_entries_already_on_the_board posts only the missing entry', sent, ['B']);
  check('history_bookmark_import_should_skip_entries_already_on_the_board drops the skipped entries', storage.getItem(HISTORY_KEY), null);
  check('history_bookmark_import_should_skip_entries_already_on_the_board shows no toast', seen, []);
});

testCase(async () => {
  const storage = storageWith(legacyList(['A', 'B']));
  const { sent, post } = replier([{ status: 500 }]);
  const { toast } = toaster();
  await BoardHistoryPage.importLegacyBookmarks(HISTORY_KEY, post, storage, toast, () => ['A']);
  check('history_bookmark_import_should_keep_an_entry_until_its_post_succeeds skips the known entry', sent, ['B']);
  check('history_bookmark_import_should_keep_an_entry_until_its_post_succeeds keeps the failed entry', names(storage.getItem(HISTORY_KEY)), ['B']);
});

testCase(async () => {
  const storage = storageWith(legacyList(['A']));
  const { sent, post } = replier([]);
  const { toast } = toaster();
  await BoardHistoryPage.importLegacyBookmarks(HISTORY_KEY, post, storage, toast, () => [], undefined);
  check('history_bookmark_import_should_run_without_navigator_locks posts the entry', sent, ['A']);
  check('history_bookmark_import_should_run_without_navigator_locks clears the key', storage.getItem(HISTORY_KEY), null);
});

cases.reduce((chain, fn) => chain.then(fn), Promise.resolve()).then(() => {
  console.log('failures=' + failures);
  process.exit(failures ? 1 : 0);
});
