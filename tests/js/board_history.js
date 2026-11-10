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

console.log('failures=' + failures);
process.exit(failures ? 1 : 0);
