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
function escapeHtml(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}
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
eval(block('  var CumulativeFlowPage = (function () {'));

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

var now = new Date(2026, 10, 15, 14, 0, 0);
function span(values) {
  var r = CumulativeFlowPage.periodRange(values, now);
  return r && [isoDate(r.start), isoDate(r.end)];
}
check('last 7 days', span({ period: '7' }), ['2026-11-09', '2026-11-15']);
check('last 30 days', span({ period: '30' }), ['2026-10-17', '2026-11-15']);
check('this month', span({ period: 'this-month' }), ['2026-11-01', '2026-11-15']);
check('last month', span({ period: 'last-month' }), ['2026-10-01', '2026-10-31']);
check('custom absolute', span({ period: 'absolute', from: '2026-10-01', to: '2026-10-10' }), ['2026-10-01', '2026-10-10']);
check('custom absolute ends no later than today', span({ period: 'absolute', from: '2026-11-01', to: '2026-12-31' }), ['2026-11-01', '2026-11-15']);
check('custom absolute needs from before to', span({ period: 'absolute', from: '2026-10-10', to: '2026-10-01' }), null);
check('custom absolute needs both dates', span({ period: 'absolute', from: '', to: '2026-10-01' }), null);
check('custom relative counts days back from today', span({ period: 'relative', days: '10' }), ['2026-11-06', '2026-11-15']);
check('custom relative needs a positive number', span({ period: 'relative', days: '0' }), null);
check('a range over three years is refused', span({ period: 'absolute', from: '2020-01-01', to: '2026-10-01' }), null);

var data = {
  columns: [
    { id: 'todo', name: 'To-do', former_names: [], is_done: false },
    { id: 'done', name: 'Done', former_names: [], is_done: true },
  ],
  tasks: [
    { id: 'a', column_id: 'todo', created_at: at(-5, 9), estimate_hours: 2.5, points: 3 },
    { id: 'b', column_id: 'done', created_at: at(-5, 9), estimate_hours: 4, points: 8 },
  ],
};
var range = { start: day(-1), end: day(0) };
var counts = CumulativeFlowPage.seriesByDay(data, data.tasks, range, 'count');
check('count mode counts tasks per column', [counts.order, counts.counts], [['To-do', 'Done'], [[1, 1], [1, 1]]]);
var hours = CumulativeFlowPage.seriesByDay(data, data.tasks, range, 'estimate');
check('time estimate mode sums hours per column', hours.counts, [[2.5, 2.5], [4, 4]]);
check('time estimate labels the axis in hours', />Time estimate \(hours\)<\/text>/.test(CumulativeFlowPage.chartSvg(hours)), true);
var points = CumulativeFlowPage.seriesByDay(data, data.tasks, range, 'points');
check('points mode sums points per column', points.counts, [[3, 3], [8, 8]]);
check('points mode labels the axis in points', />Points<\/text>/.test(CumulativeFlowPage.chartSvg(points)), true);
check('count mode labels the axis', />Task count<\/text>/.test(CumulativeFlowPage.chartSvg(counts)), true);

counts.hidden = { 'To-do': true };
check('a toggled off column leaves the tooltip', CumulativeFlowPage.tipFor(counts, 0).rows.map(function (r) { return r.label; }), ['Done']);
check('a toggled off column leaves the chart', /To-do/.test(CumulativeFlowPage.chartSvg(counts)), false);
counts.hidden = {};
check('a shown column is in the tooltip', CumulativeFlowPage.tipFor(counts, 0).rows.map(function (r) { return r.label; }), ['To-do', 'Done']);
check('the tooltip rows carry no toggle control', /data-band|button/.test(JSON.stringify(CumulativeFlowPage.tipFor(counts, 0))), false);

var controls = CumulativeFlowPage.controls(data);
function control(id) { return controls.filter(function (c) { return c.id === id; })[0]; }
check('period choices', control('period').options.map(function (o) { return o.text; }),
  ['Last 7 days', 'Last 30 days', 'This month', 'Last month', 'Custom absolute', 'Custom relative']);
check('value types', control('valueType').options.map(function (o) { return o.text; }), ['Task count', 'Time estimate', 'Points']);
check('the pickers show for their period only',
  [control('from').when, control('to').when, control('days').when].map(function (w) { return w.value; }),
  ['absolute', 'absolute', 'relative']);

var handlers = {};
var picker = { hidden: true, getAttribute: function (n) { return n === 'data-when' ? 'period' : 'absolute'; } };
var host = {
  innerHTML: '',
  addEventListener: function (type, fn) { handlers[type] = fn; },
  querySelectorAll: function (sel) { return sel === '[data-when]' ? [picker] : []; },
};
ReportFilter.mount(host, ReportFilter.create(controls, function () {}));
check('a picker starts hidden', /data-when-value="absolute" hidden/.test(host.innerHTML), true);
handlers.change({ target: { getAttribute: function () { return 'period'; }, type: 'select-one', value: 'absolute' } });
check('choosing its period shows the picker', picker.hidden, false);
handlers.change({ target: { getAttribute: function () { return 'period'; }, type: 'select-one', value: '7' } });
check('choosing another period hides it again', picker.hidden, true);

page['cumulative-flow-status'] = { textContent: '' };
page['cumulative-flow-chart'] = { innerHTML: 'old' };
page['cumulative-flow-legend'] = { innerHTML: 'old' };
CumulativeFlowPage.render(data, { period: 'absolute', from: '2026-10-10', to: '2026-10-01', valueType: 'count',
  user: '', color: '', labels: [], labelMode: 'and' });
check('an invalid period clears the chart, legend and says why',
  [page['cumulative-flow-status'].textContent, page['cumulative-flow-chart'].innerHTML, page['cumulative-flow-legend'].innerHTML],
  ['Pick a valid period of at most 3 years that starts on or before today.', '', '']);

if (failures) process.exit(1);
console.log('cumulative_flow_filter: all checks passed');
