var fs = require('fs');
var src = fs.readFileSync(process.argv[2], 'utf8');
function block(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  var to = src.indexOf('\n  })();', from) + '\n  })();'.length;
  return src.slice(from, to);
}
var document = { getElementById: function () { return null; } };
function escapeHtml(s) { return String(s); }
const { isoDate } = require('../../static/pure.js');
eval(block('  var ReportBreakdown = (function () {'));
eval(block('  var TimeEstimatePage = (function () {'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

var now = new Date(2026, 10, 15, 13, 0);
function at(y, m, d) { return new Date(y, m - 1, d, 12).toISOString(); }
var data = {
  columns: [{ id: 'todo', name: 'To do' }, { id: 'done', name: 'Done' }],
  colors: [{ id: 'red', value: '#f00', label: 'Urgent' }],
  users: [{ id: 'u1', username: 'ann' }, { id: 'u2', username: 'bob' }],
  tasks: [
    { id: 'a', column_id: 'todo', color_id: 'red', labels: ['x', 'y'], member_ids: ['u1', 'u2'], responsible: { kind: 'member', id: 'u1' }, estimate_hours: 1.5 },
    { id: 'b', column_id: 'todo', color_id: null, labels: [], member_ids: [], responsible: null, estimate_hours: 4.5 },
    { id: 'c', column_id: 'done', color_id: 'red', labels: ['x'], member_ids: ['u2'], responsible: { kind: 'session', name: 'agent-1' }, estimate_hours: 2, completed_at: at(2026, 11, 13) },
    { id: 'd', column_id: 'done', color_id: 'red', labels: ['x'], member_ids: ['u2'], responsible: null, estimate_hours: 3, completed_at: at(2026, 10, 1) },
  ],
};
function rowsOf(group, extra) {
  var opts = Object.assign({ group: group, range: ReportBreakdown.periodRange('7', '', '', now), user: '', color: '', label: '' }, extra || {});
  return ReportBreakdown.build(data, opts, function () { return 1; }).rows.map(function (r) { return [r.label].concat(r.sums); });
}

check('group by total gives one row', rowsOf('total'), [['Total', 2, 1]]);
check('group by colour', rowsOf('color'), [['Urgent', 1, 1], ['No color', 1, 0]]);
check('group by label counts a task under each label', rowsOf('label'), [['x', 1, 1], ['y', 1, 0], ['No label', 1, 0]]);
check('group by responsible', rowsOf('responsible'), [['ann', 1, 0], ['Unassigned', 1, 0], ['agent-1', 0, 1]]);
check('group by user counts a task for each member', rowsOf('user'), [['ann', 1, 0], ['bob', 1, 1], ['Unassigned', 1, 0]]);
check('an empty board still shows the Total row',
  ReportBreakdown.build({ columns: data.columns, tasks: [] }, { group: 'total', range: null }, function () { return 1; }).rows.length, 1);

check('a longer period brings back older archived tasks', rowsOf('total', { range: ReportBreakdown.periodRange('90', '', '', now) }), [['Total', 2, 2]]);
check('last month', rowsOf('total', { range: ReportBreakdown.periodRange('last-month', '', '', now) }), [['Total', 2, 1]]);
check('this month', rowsOf('total', { range: ReportBreakdown.periodRange('this-month', '', '', now) }), [['Total', 2, 1]]);
check('custom period', rowsOf('total', { range: ReportBreakdown.periodRange('custom', '2026-10-01', '2026-10-01', now) }), [['Total', 2, 1]]);
check('custom period with the end before the start', ReportBreakdown.periodRange('custom', '2026-10-02', '2026-10-01', now), null);
check('user filter', rowsOf('total', { user: 'u1' }), [['Total', 1, 0]]);
check('unassigned user filter', rowsOf('total', { user: 'none' }), [['Total', 1, 0]]);
check('colour filter', rowsOf('total', { color: 'red' }), [['Total', 1, 1]]);
check('no-colour filter', rowsOf('total', { color: 'none' }), [['Total', 1, 0]]);
check('label filter', rowsOf('total', { label: 'y' }), [['Total', 1, 0]]);

var est = ReportBreakdown.build(data, { group: 'color', range: ReportBreakdown.periodRange('7', '', '', now), user: '', color: '', label: '' },
  function (t) { return t.estimate_hours || 0; });
check('estimate table sums hours', est.rows.map(function (r) { return r.sums; }), [[1.5, 2], [4.5, 0]]);
check('estimate export, human readable', TimeEstimatePage.exportRows(est, false), [['Color', 'To do', 'Done'], ['Urgent', '1h 30m', '2h 00m'], ['No color', '4h 30m', '0h 00m']]);
check('estimate export, total seconds', TimeEstimatePage.exportRows(est, true), [['Color', 'To do', 'Done'], ['Urgent', 5400, 7200], ['No color', 16200, 0]]);
check('estimate prints six hours as 6h 00m', TimeEstimatePage.fmtEstimate(6), '6h 00m');

var multi = ReportBreakdown.build({ columns: data.columns, tasks: [data.tasks[0]] },
  { group: 'label', range: null, user: '', color: '', label: '' }, function (t) { return t.estimate_hours; });
check('a task under two labels counts its hours once in the total', multi.total, 1.5);
var odd = ReportBreakdown.build({ columns: data.columns, tasks: [{ id: 'z', column_id: 'todo', labels: ['constructor'] }] },
  { group: 'label', range: null, user: '', color: '', label: '' }, function () { return 1; });
check('a label named constructor gets its own row', odd.rows.map(function (r) { return [r.label, r.sums[0]]; }), [['constructor', 1]]);

var tc = fs.readFileSync('templates/_report_breakdown.html', 'utf8');
check('five Group by values, Color the default', [(tc.match(/<option value="(total|color|label|responsible|user)"/g) || []).length, /value="color" selected/.test(tc)], [5, true]);
check('Last 7 days is the default period', /value="7" selected/.test(tc), true);
check('both reports include the sidebar', ['task_count', 'time_estimate'].every(function (n) {
  return fs.readFileSync('templates/' + n + '.html', 'utf8').indexOf('_report_breakdown.html') > 0;
}), true);

console.log('failures=' + failures);
if (failures) throw new Error(failures + ' report breakdown check(s) failed');
