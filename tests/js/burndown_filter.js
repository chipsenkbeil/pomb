var fs = require('fs');
var src = fs.readFileSync(process.argv[2], 'utf8');
const { isoDate } = require('../../static/pure.js');
function block(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  var to = src.indexOf('\n  })();', from) + '\n  })();'.length;
  return src.slice(from, to);
}
var page = {};
var document = {
  getElementById: function (id) {
    if (!page[id]) page[id] = { innerHTML: '', textContent: '', addEventListener: function () {} };
    return page[id];
  },
};
function escapeHtml(s) { return String(s); }
var accountPrefs = { hour12: true, weekStart: 1 };
function fn(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}
eval(fn('  function daysIntoWeek(d) {'));
eval(fn('  function clockText(d) {'));
eval(block('  var ReportChart = (function () {'));
eval(block('  var ReportBreakdown = (function () {'));
eval(block('  var ColumnHistory = (function () {'));
eval(block('  var ReportRange = (function () {'));
eval(block('  var ReportValue = (function () {'));
eval(block('  var ReportFilter = (function () {'));
eval(block('  var BurndownPage = (function () {'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function at(offset, h) {
  var d = new Date();
  d.setDate(d.getDate() + offset);
  d.setHours(h, 0, 0, 0);
  return d.toISOString();
}
function day(offset) {
  var d = new Date();
  d.setDate(d.getDate() + offset);
  return new Date(d.getFullYear(), d.getMonth(), d.getDate());
}

var data = {
  columns: [
    { id: 'todo', name: 'To-do', former_names: [], is_done: false },
    { id: 'doing', name: 'Doing', former_names: [], is_done: false },
    { id: 'done', name: 'Finished', former_names: [], is_done: true },
  ],
  tasks: [
    { id: 'a', column_id: 'todo', created_at: at(-10, 9), estimate_hours: 2, points: 3 },
    { id: 'b', column_id: 'doing', created_at: at(-10, 9), estimate_hours: 3, points: 5 },
    { id: 'c', column_id: 'done', created_at: at(-2, 9), completed_at: at(-1, 12), estimate_hours: 5 },
  ],
};
function counts(extra) {
  var opts = { start: day(-3), valueType: 'count', target: 'done', targetDone: true, from: null,
    cols: ColumnHistory.lookup(data) };
  Object.keys(extra || {}).forEach(function (k) { opts[k] = extra[k]; });
  return BurndownPage.remainingByDay(data.tasks, opts).counts;
}

check('count mode counts open tasks per day from the start date', counts(), [2, 3, 2, 2]);
check('time estimate mode sums hours', counts({ valueType: 'estimate' }), [5, 10, 5, 5]);
check('points mode sums points', counts({ valueType: 'points' }), [8, 8, 8, 8]);
check('from columns keep only tasks in them', counts({ from: ['doing'] }), [1, 1, 1, 1]);
check('a target column that is not done counts tasks sitting in it as finished',
  counts({ target: 'doing', targetDone: false }), [1, 2, 2, 2]);

var controls = BurndownPage.controls(data);
function control(id) { return controls.filter(function (c) { return c.id === id; })[0]; }
check('the default target is the done column', control('target').value, 'done');
check('the done column is found by flag, not name', control('target').options[2].text, 'Finished');
check('from columns default to every column', control('from').value, ['todo', 'doing', 'done']);
check('value types offered', control('valueType').options.map(function (o) { return o.text; }),
  ['Task count', 'Time estimate', 'Points']);
check('the default start date is 30 days ago', control('start').value, isoDate(day(-30)));

var series = BurndownPage.remainingByDay(data.tasks, { start: day(-3), valueType: 'estimate',
  target: 'done', targetDone: true, from: null, cols: ColumnHistory.lookup(data) });
check('estimate axis label', />Time estimate \(hours\)<\/text>/.test(BurndownPage.chartSvg(series)), true);
var count = BurndownPage.remainingByDay(data.tasks, { start: day(-3), valueType: 'count',
  target: 'done', targetDone: true, from: null, cols: ColumnHistory.lookup(data) });
check('count axis label', />Task count<\/text>/.test(BurndownPage.chartSvg(count)), true);
check('estimate tooltip says hours', BurndownPage.tipFor(series, 1).rows[0].label, 'Hours remaining');
var pointSeries = BurndownPage.remainingByDay(data.tasks, { start: day(-3), valueType: 'points',
  target: 'done', targetDone: true, from: null, cols: ColumnHistory.lookup(data) });
check('points axis label', />Points<\/text>/.test(BurndownPage.chartSvg(pointSeries)), true);
check('points tooltip says points', BurndownPage.tipFor(pointSeries, 1).rows[0].label, 'Points remaining');

var burn = [];
for (var q = 0; q < 6; q++) burn.push({ id: 'o' + q, column_id: 'todo', created_at: at(-10, 9) });
for (var r = 0; r < 4; r++) burn.push({ id: 'n' + r, column_id: 'done', created_at: at(-12, 9), completed_at: at(-1, 12) });
for (var u = 0; u < 3; u++) burn.push({ id: 'p' + u, column_id: 'done', created_at: at(-20, 9), completed_at: at(-10, 12) });
function projected(basis) {
  var s = BurndownPage.remainingByDay(burn, { start: day(-3), valueType: 'count', basis: basis,
    target: 'done', targetDone: true, from: null, cols: ColumnHistory.lookup(data) });
  return s.projection[0];
}
check('a 7 day basis burns 4 tasks over 7 days', projected(7), 6 - 4 / 7);
check('a 14 day basis burns 7 tasks over 14 days', projected(14), 6 - 7 / 14);
check('without a basis the 30 day default applies', projected(undefined), 6 - 7 / 30);
check('basis choices', control('basis').options.map(function (o) { return o.value; }),
  ['7', '14', '21', '30', '60', '90', '120']);
check('the default basis is 30 days', control('basis').value, '30');
var line = BurndownPage.chartSvg(BurndownPage.remainingByDay(burn, { start: day(-3), valueType: 'count',
  basis: 7, target: 'done', targetDone: true, from: null, cols: ColumnHistory.lookup(data) }));
check('the projection is drawn as a polyline', /<polyline[^>]*class="chart-projection"/.test(line), true);
check('the stylesheet dashes the projection line',
  /\.chart-projection\s*\{[^}]*stroke-dasharray:\s*6 4/.test(fs.readFileSync('static/style.css', 'utf8')), true);

var routed = [];
for (var w = 0; w < 6; w++) routed.push({ id: 'r' + w, column_id: 'doing', created_at: at(-10, 9) });
for (var z = 0; z < 4; z++) {
  routed.push({ id: 'm' + z, column_id: 'done', created_at: at(-12, 9), completed_at: at(-1, 12), history: [
    { kind: 'created', detail: 'Task created in To-do', column_id: 'todo', created_at: at(-12, 9) },
    { kind: 'moved', detail: 'Moved from To-do to Finished', column_id: 'done', created_at: at(-1, 12) },
  ] });
}
function routedProjection(from) {
  return BurndownPage.remainingByDay(routed, { start: day(-3), valueType: 'count', basis: 7, target: 'done',
    targetDone: true, from: from, cols: ColumnHistory.lookup(data) }).projection;
}
check('the rate counts tasks that came from a From column', routedProjection(['todo', 'doing']).length > 0, true);
check('the first projected day uses that rate', routedProjection(['todo', 'doing'])[0], 6 - 4 / 7);
check('completions that never sat in a From column add no rate', routedProjection(['doing']), []);
function status(patch) {
  var values = { valueType: 'count', target: 'done', from: ['todo', 'doing', 'done'], start: isoDate(day(-3)),
    basis: '30', user: '', color: '', labels: [], labelMode: 'and' };
  Object.keys(patch).forEach(function (k) { values[k] = patch[k]; });
  page['burndown-status'] = { textContent: '' };
  page['burndown-chart'] = { innerHTML: 'old' };
  BurndownPage.render(data, values);
  return [page['burndown-status'].textContent, page['burndown-chart'].innerHTML];
}
check('a future start date draws nothing', status({ start: isoDate(day(2)) }),
  ['Pick a start date on or before today.', '']);
check('a start date older than 3 years draws nothing', status({ start: isoDate(day(-1200)) }),
  ['Pick a start date within the last 3 years.', '']);
check('an empty From selection draws nothing', status({ from: [] }), ['Choose at least one From column.', '']);
check('an empty start date draws nothing', status({ start: '' }), ['Pick a start date on or before today.', '']);

if (failures) process.exit(1);
console.log('burndown_filter: all checks passed');
