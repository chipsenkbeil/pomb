var fs = require('fs');
var src = fs.readFileSync(process.argv[2], 'utf8');
function block(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  var to = src.indexOf('\n  })();', from) + '\n  })();'.length;
  return src.slice(from, to);
}
function fn(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}
var page = {};
var document = {
  getElementById: function (id) {
    if (!page[id]) page[id] = { innerHTML: '', textContent: '', addEventListener: function () {} };
    return page[id];
  },
};
function escapeHtml(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}
var accountPrefs = { hour12: true, weekStart: 1 };
eval(fn('  function daysIntoWeek(d) {'));
eval(fn('  function clockText(d) {'));
const { isoDate } = require('../../static/pure.js');
eval(block('  var ReportChart = (function () {'));
eval(block('  var ReportBreakdown = (function () {'));
eval(block('  var ColumnHistory = (function () {'));
eval(block('  var ReportRange = (function () {'));
eval(block('  var ReportValue = (function () {'));
eval(block('  var ReportFilter = (function () {'));
eval(block('  var ThroughputPage = (function () {'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

var now = new Date(2026, 10, 15, 14, 0, 0);
function span(values) {
  var r = ThroughputPage.periodRange(values, now);
  return r && [isoDate(r.start), isoDate(r.end)];
}
check('last 12 weeks starts on the week start 11 weeks back', span({ period: '12w' }), ['2026-08-24', '2026-11-15']);
check('last 24 weeks', span({ period: '24w' }), ['2026-06-01', '2026-11-15']);
check('this year', span({ period: 'this-year' }), ['2026-01-01', '2026-11-15']);
check('last year', span({ period: 'last-year' }), ['2025-01-01', '2025-12-31']);
check('custom absolute', span({ period: 'absolute', from: '2026-10-01', to: '2026-10-10' }), ['2026-10-01', '2026-10-10']);
check('custom relative', span({ period: 'relative', days: '10' }), ['2026-11-06', '2026-11-15']);
check('a range over three years is refused', span({ period: 'absolute', from: '2020-01-01', to: '2026-10-01' }), null);

function done(id, day, owner, extra) {
  var task = {
    id: id, column_id: 'done', created_at: '2026-09-01T09:00:00', estimate_hours: 2, labels: [], color_id: '',
    owner: owner || null,
    history: [
      { kind: 'created', detail: 'Task created in To do', column_id: 'todo', created_at: '2026-09-01T09:00:00' },
      { kind: 'moved', detail: 'Moved from To do to Done', column_id: 'done', created_at: '2026-' + day + 'T12:00:00' },
    ],
  };
  Object.keys(extra || {}).forEach(function (k) { task[k] = extra[k]; });
  return task;
}
var ana = { holder: { kind: 'human', id: 'u1' } };
var bot = { holder: { kind: 'agent', token_id: null, name: 'builder' } };
var data = {
  columns: [{ id: 'todo', name: 'To do', is_done: false }, { id: 'done', name: 'Done', is_done: true }],
  users: [{ id: 'u1', username: 'ana' }],
  colors: [],
  tasks: [
    done('a', '09-08', ana), done('b', '09-16', bot), done('c', '09-17', null), done('d', '09-23', ana),
    done('e', '10-01', ana), done('f', '10-02', null), done('g', '10-03', bot), done('h', '10-12', ana),
  ],
};
var base = {
  valueType: 'count', target: 'done', period: 'absolute', from: '2026-09-07', to: '2026-10-18',
  user: '', color: '', labels: [], labelMode: 'and', group: 'week',
};
function run(over) {
  var values = {};
  Object.keys(base).forEach(function (k) { values[k] = base[k]; });
  Object.keys(over).forEach(function (k) { values[k] = over[k]; });
  return ThroughputPage.series(data, values, now);
}
function counts(over) { return run(over).values; }
function starts(over) { return run(over).starts.map(isoDate); }

check('week buckets', counts({ group: 'week' }), [1, 2, 1, 3, 0, 1]);
check('2 week buckets', counts({ group: '2w' }), [3, 4, 1]);
check('2 week bucket starts', starts({ group: '2w' }), ['2026-09-07', '2026-09-21', '2026-10-05']);
check('3 week buckets', counts({ group: '3w' }), [4, 4]);
check('3 week bucket starts', starts({ group: '3w' }), ['2026-09-07', '2026-09-28']);
check('4 week buckets', counts({ group: '4w' }), [7, 1]);
check('4 week bucket starts', starts({ group: '4w' }), ['2026-09-07', '2026-10-05']);
check('month buckets', counts({ group: 'month' }), [4, 4]);
check('month bucket starts', starts({ group: 'month' }), ['2026-09-01', '2026-10-01']);
check('day buckets cover every day of the period', counts({ group: 'day' }).length, 42);
check('day buckets total the arrivals', counts({ group: 'day' }).reduce(function (a, b) { return a + b; }, 0), 8);
check('time estimate sums the estimates', counts({ group: '2w', valueType: 'estimate' }), [6, 8, 2]);
check('an arrival before the period is not counted', counts({ from: '2026-09-10', group: 'day' }).reduce(function (a, b) { return a + b; }, 0), 7);

check('user filter keeps a human owner', counts({ user: 'u:u1', group: '4w' }), [3, 1]);
check('user filter keeps an agent owner', counts({ user: 'a:builder', group: '4w' }), [2, 0]);
check('user filter keeps unowned tasks', counts({ user: 'unassigned', group: '4w' }), [2, 0]);
check('user filter ignores members', ThroughputPage.series({
  columns: data.columns, users: data.users,
  tasks: [done('m', '09-08', null, { member_ids: ['u1'] })],
}, { valueType: 'count', target: 'done', period: 'absolute', from: '2026-09-07', to: '2026-09-13',
  user: 'u:u1', color: '', labels: [], labelMode: 'and', group: 'week' }, now).values, [0]);
check('color filter', counts({ color: 'red', group: '4w' }), [0, 0]);
check('target column picks the arrivals', counts({ target: 'todo', from: '2026-08-25', group: '4w' }), [8, 0]);

var controls = ThroughputPage.controls(data, now);
check('the panel offers every option in order', controls.map(function (c) { return c.id; }),
  ['valueType', 'target', 'period', 'from', 'to', 'days', 'user', 'color', 'labels', 'labelMode', 'group']);
check('target defaults to the done column', controls[1].value, 'done');
check('user options hold the owners', controls[6].options.map(function (o) { return o.value; }),
  ['', 'unassigned', 'u:u1', 'a:builder']);
check('group options', controls[10].options.map(function (o) { return o.text; }),
  ['Day', 'Week', '2 weeks', '3 weeks', '4 weeks', 'Month']);

var s = run({ group: '2w', valueType: 'estimate' });
check('tooltip holds the date and the value', ThroughputPage.tipFor(s, 1),
  { title: '21 September 12:00 AM', rows: [{ label: 'Time estimate', value: '8 hours' }] });
check('tooltip counts tasks', ThroughputPage.tipFor(run({ group: '2w' }), 2),
  { title: '5 October 12:00 AM', rows: [{ label: 'Task count', value: '1 task' }] });

if (failures) process.exit(1);
console.log('throughput_options: all checks passed');
