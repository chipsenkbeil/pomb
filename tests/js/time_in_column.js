var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var TimeInColumnPage = (function () {');
var end = src.indexOf('\n  })();', start) + '\n  })();'.length;
if (start < 0 || end < start) throw new Error('TimeInColumnPage not found');
var document = { getElementById: function () { return null; } };
function escapeHtml(s) { return String(s); }
eval(src.slice(start, end));

var MIN = 60000, HOUR = 60 * MIN, DAY = 24 * HOUR;
var NOW = Date.parse('2026-02-01T00:00:00Z');
function iso(ms) { return new Date(ms).toISOString(); }

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

var columns = [
  { id: 'todo', name: 'To-do', former_names: [] },
  { id: 'rev', name: 'Reviewing', former_names: ['In Review'] },
  { id: 'done', name: 'Done', former_names: [] }
];
function ev(kind, detail, col, ms) {
  return { kind: kind, detail: detail, column_id: col, created_at: iso(ms) };
}
var a = {
  id: 'a', name: 'A', column_id: 'done', created_at: iso(NOW - 10 * DAY),
  color_id: 'red', labels: ['bug'], member_ids: ['u1'],
  history: [
    ev('created', 'Task created in To-do', 'todo', NOW - 10 * DAY),
    ev('moved', 'Moved from To-do to In Review', null, NOW - 6 * DAY),
    ev('moved', 'Moved from In Review to Done', 'done', NOW - 4 * DAY - 3 * HOUR - 7 * MIN)
  ]
};
var b = {
  id: 'b', name: 'B', column_id: 'todo', created_at: iso(NOW - 2 * DAY),
  color_id: null, labels: [], member_ids: [], history: []
};
var data = { columns: columns, tasks: [a, b] };
var all = { columns: null, periodDays: null, user: '', color: '', label: '' };
function opts(o) { return Object.assign({}, all, o); }
function row(t, name) { return t.rows.filter(function (r) { return r.name === name; })[0]; }

var t = TimeInColumnPage.buildTable(data, all, NOW);
check('columns follow the board', t.columns.map(function (c) { return c.id; }), ['todo', 'rev', 'done']);
check('sums per column, current column up to now', row(t, 'A').ms,
  { todo: 4 * DAY, rev: 2 * DAY - 3 * HOUR - 7 * MIN, done: 4 * DAY + 3 * HOUR + 7 * MIN });
check('task without moves sits in its column since creation', row(t, 'B').ms, { todo: 2 * DAY });

var renamed = JSON.parse(JSON.stringify(data));
renamed.columns.push({ id: 'new', name: 'Added later', former_names: [] });
check('runtime-added column appears',
  TimeInColumnPage.buildTable(renamed, all, NOW).columns.map(function (c) { return c.name; }),
  ['To-do', 'Reviewing', 'Done', 'Added later']);

check('column filter narrows columns',
  TimeInColumnPage.buildTable(data, opts({ columns: ['rev'] }), NOW).columns.map(function (c) { return c.id; }),
  ['rev']);
check('user filter', TimeInColumnPage.buildTable(data, opts({ user: 'u1' }), NOW).rows.map(function (r) { return r.name; }), ['A']);
check('color filter', TimeInColumnPage.buildTable(data, opts({ color: 'red' }), NOW).rows.map(function (r) { return r.name; }), ['A']);
check('no-color filter', TimeInColumnPage.buildTable(data, opts({ color: 'none' }), NOW).rows.map(function (r) { return r.name; }), ['B']);
check('label filter', TimeInColumnPage.buildTable(data, opts({ label: 'bug' }), NOW).rows.map(function (r) { return r.name; }), ['A']);

var p = TimeInColumnPage.buildTable(data, opts({ periodDays: 1 }), NOW);
check('period clips to the window', [row(p, 'A').ms, row(p, 'B').ms], [{ done: DAY }, { todo: DAY }]);

var ms = 27 * DAY + 3 * HOUR + 7 * MIN + 40000;
check('minutes', TimeInColumnPage.fmtDuration(ms, 'minutes'), '27d 03h 08m');
check('hours', TimeInColumnPage.fmtDuration(ms, 'hours'), '27d 03h');
check('days', TimeInColumnPage.fmtDuration(ms, 'days'), '27d');
check('zero minutes', TimeInColumnPage.fmtDuration(0, 'minutes'), '0m');

console.log('failures=' + failures);
if (failures) throw new Error(failures + ' time in column check(s) failed');
