var src = require('fs').readFileSync(process.argv[2], 'utf8');
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
var currentDocument = { getElementById: function () { return null; }, querySelector: function () { return null; } };
var document = {
  getElementById: function () { return null; },
  createElement: function () { return { style: {}, offsetWidth: 100, hidden: false }; },
  querySelector: function (sel) { return currentDocument.querySelector(sel); },
};
function escapeHtml(s) { return String(s); }
var accountPrefs = { hour12: true, weekStart: 1 };
eval(fn('  function daysIntoWeek(d) {'));
eval(fn('  function clockText(d) {'));
eval(block('  var ReportChart = (function () {'));
eval(block('  var ThroughputPage = (function () {'));
eval(block('  var ReportRange = (function () {'));
eval(block('  var ReportValue = (function () {'));
eval(block('  var BurndownPage = (function () {'));
eval(block('  var CycleLeadPage = (function () {'));
eval(block('  var ColumnHistory = (function () {'));
eval(block('  var CumulativeFlowPage = (function () {'));
eval(fn('  function reportBackHref() {'));
eval(fn('  function reportEscapeHref() {'));
eval(fn('  function boardId() {'));
eval(fn('  function modalBoardId() {'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function ticks(svg) {
  var out = [];
  var re = /class="chart-tick">([^<]*)<\/text>/g, m;
  while ((m = re.exec(svg))) out.push(m[1]);
  return out;
}
function titles(svg) {
  var out = [];
  var re = /<title>([^<]*)<\/title>/g, m;
  while ((m = re.exec(svg))) out.push(m[1]);
  return out;
}
var SHORT = /^[A-Z][a-z]{2} \d{1,2}$/;
var LONG = /\d{1,2} [A-Z][a-z]+ \d{1,2}:\d{2} (AM|PM)$/;

check('short date', ReportChart.shortDate(new Date(2026, 9, 1)), 'Oct 1');
check('short date, two-digit day', ReportChart.shortDate(new Date(2026, 0, 21)), 'Jan 21');
check('long date at midnight', ReportChart.longDate(new Date(2026, 9, 21, 0, 0)), '21 October 12:00 AM');
check('long date in the afternoon', ReportChart.longDate(new Date(2026, 9, 21, 15, 5)), '21 October 3:05 PM');
check('long date at noon', ReportChart.longDate(new Date(2026, 9, 21, 12, 30)), '21 October 12:30 PM');
check('day start drops the time', ReportChart.longDate(ReportChart.dayStart(new Date(2026, 9, 21, 23, 59, 59, 999))), '21 October 12:00 AM');

var week = []; var ends = [];
for (var i = 0; i < 12; i++) week.push(new Date(2026, 8, 7 + 7 * i));
for (var j = 0; j < 30; j++) { var e = new Date(2026, 9, 1 + j, 23, 59, 59, 999); ends.push(e); }

var tpSeries = { starts: week, values: week.map(function () { return 1; }), type: 'count', group: 'week' };
var tp = ThroughputPage.chartSvg(tpSeries);
check('throughput axis labels use the short form', ticks(tp).slice(6).every(function (t) { return SHORT.test(t); }), true);
check('throughput tooltips use the long form', LONG.test(ThroughputPage.tipFor(tpSeries, 0).title), true);

var bd = BurndownPage.chartSvg({ ends: ends, counts: ends.map(function () { return 2; }) });
check('burndown axis labels use the short form', ticks(bd).slice(6).every(function (t) { return SHORT.test(t); }), true);
check('burndown tooltips use the long form at the day start', titles(bd)[0], '2 tasks remaining, 1 October 12:00 AM');
check('burndown crosshair tooltip lists the remaining count', BurndownPage.tipFor({ ends: ends, counts: ends.map(function (_, k) { return k; }) }, 3),
  { title: '4 October 12:00 AM', rows: [{ label: 'Tasks remaining', value: 3 }] });
check('burndown draws a hidden crosshair', /<line class="chart-crosshair"[^>]*visibility="hidden"/.test(bd), true);

var VIEW = { metric: 'cycle', hidden: {} };
var cl = CycleLeadPage.chartSvg({ starts: week, avg: week.map(function () { return { cycle: 1, lead: 2, n: 1 }; }) }, VIEW);
check('cycle axis labels use the short form', ticks(cl).slice(6).every(function (t) { return SHORT.test(t); }), true);

var cf = CumulativeFlowPage.chartSvg({ ends: ends, order: ['A'], counts: [ends.map(function () { return 1; })] });
check('cumulative flow axis labels use the short form', ticks(cf).slice(6).every(function (t) { return SHORT.test(t); }), true);

function daysAgo(n, h) {
  var d = new Date();
  d.setDate(d.getDate() - n);
  d.setHours(h, 0, 0, 0);
  return d.toISOString();
}
var cyc = CycleLeadPage.bucketize(CycleLeadPage.computeTimes([
  { id: 't1', name: 'Fix login', created_at: daysAgo(20, 9), completed_at: daysAgo(2, 9),
    history: [{ kind: 'moved', created_at: daysAgo(10, 9) }] },
  { id: 't2', name: 'Open task', created_at: daysAgo(5, 9), completed_at: null, history: [] },
]));
function dots(svg) { return svg.match(/<circle[^>]*class="chart-task-dot"[^>]*>/g) || []; }
function attr(el, name) { var m = new RegExp(' ' + name + '="([^"]*)"').exec(el); return m && m[1]; }
var cycleDots = dots(CycleLeadPage.chartSvg(cyc, { metric: 'cycle', hidden: {} }));
check('cycle time draws one dot per completed task', cycleDots.length, 1);
check('cycle dot names the task', attr(cycleDots[0], 'data-name'), 'Fix login');
check('cycle dot carries the task id for the click', attr(cycleDots[0], 'data-task-id'), 't1');
check('cycle dot ends at the completion', attr(cycleDots[0], 'data-to'), ReportChart.longDate(new Date(daysAgo(2, 9))));
var leadDots = dots(CycleLeadPage.chartSvg(cyc, { metric: 'lead', hidden: {} }));
check('lead time draws the same task', leadDots.length, 1);
check('lead dot counts days from creation', attr(leadDots[0], 'data-days'), '18 days');
check('lead dot starts at creation', attr(leadDots[0], 'data-from'), ReportChart.longDate(new Date(daysAgo(20, 9))));

var tipHandlers = {};
var tipBox = null;
var opened = [];
var tipHost = {
  querySelector: function () { return tipBox; },
  appendChild: function (el) { tipBox = el; },
  addEventListener: function (type, fn) { tipHandlers[type] = fn; },
  getBoundingClientRect: function () { return { left: 0, top: 0, width: 400, height: 200 }; },
};
ReportChart.attachDots(tipHost, '.chart-task-dot', function (id) { opened.push(id); });
var dotEl = { dataset: { taskId: 't1', name: 'Fix login', from: '1 October 9:00 AM', to: '9 October 9:00 AM', days: '8 days' } };
var dotEvent = { target: { closest: function () { return dotEl; } }, clientX: 10, clientY: 10, key: 'Enter', preventDefault: function () {} };
tipHandlers.pointerover(dotEvent);
check('hovering a dot shows its name, From, To and Days', tipBox.innerHTML.replace(/<[^>]*>/g, '|').replace(/\|+/g, '|'),
  '|Fix login|From|1 October 9:00 AM|To|9 October 9:00 AM|Days|8 days|');
check('hovering a dot shows the tooltip', tipBox.hidden, false);
tipHandlers.pointerout(dotEvent);
check('leaving a dot hides the tooltip', tipBox.hidden, true);
tipHandlers.pointerover({ target: { closest: function () { return null; } }, clientX: 0, clientY: 0 });
check('hovering the plot away from a dot shows nothing', tipBox.hidden, true);
tipHandlers.click(dotEvent);
tipHandlers.keydown(dotEvent);
check('clicking or pressing Enter on a dot opens its task', opened, ['t1', 't1']);

var hiddenSvg = CycleLeadPage.chartSvg(cyc, { metric: 'cycle', hidden: { metric: true } });
check('hiding the metric series drops its line, dots and task dots',
  [/class="chart-line"/.test(hiddenSvg), /class="chart-dot"/.test(hiddenSvg), dots(hiddenSvg).length], [false, false, 0]);
check('hiding the metric series keeps the trend', /class="chart-trend"/.test(hiddenSvg), true);
var noTrend = CycleLeadPage.chartSvg(cyc, { metric: 'cycle', hidden: { trend: true } });
check('hiding the trend drops only the trend',
  [/class="chart-trend"/.test(noTrend), /class="chart-line"/.test(noTrend), dots(noTrend).length], [false, true, 1]);
check('showing the series again restores it', dots(CycleLeadPage.chartSvg(cyc, { metric: 'cycle', hidden: { metric: false } })).length, 1);

currentDocument = { querySelector: function (sel) {
  return sel === '.bshell-report .bshell-back' ? { getAttribute: function () { return '/b/abc'; } } : null;
} };
check('a report page escapes to its board', reportBackHref(), '/b/abc');
currentDocument = { querySelector: function () { return null; } };
check('another page has no report target', reportBackHref(), null);

function pageWith(nodes) {
  return { querySelector: function (sel) { return nodes[sel] || null; } };
}
var backLink = { getAttribute: function () { return '/b/abc'; } };
currentDocument = pageWith({ '.bshell-report .bshell-back': backLink });
check('Esc on a report with no modal leaves for the board', reportEscapeHref(), '/b/abc');
currentDocument = pageWith({ '.bshell-report .bshell-back': backLink, '#modal-root .task-modal': {} });
check('Esc with the task modal open stays on the report', reportEscapeHref(), null);
currentDocument = pageWith({});
check('Esc on another page does not navigate', reportEscapeHref(), null);

var times = CycleLeadPage.computeTimes([
  { id: 'a', name: 'Moved late', created_at: '2026-01-01T09:00:00Z', completed_at: '2026-01-09T09:00:00Z',
    history: [{ kind: 'moved', created_at: '2026-01-07T09:00:00Z' }, { kind: 'moved', created_at: '2026-01-05T09:00:00Z' }, { kind: 'created', created_at: '2026-01-02T09:00:00Z' }] },
  { id: 'b', name: 'Never moved', created_at: '2026-01-01T09:00:00Z', completed_at: '2026-01-09T09:00:00Z', history: [] },
  { id: 'c', name: 'Moved before creation', created_at: '2026-01-03T09:00:00Z', completed_at: '2026-01-09T09:00:00Z',
    history: [{ kind: 'moved', created_at: '2026-01-02T09:00:00Z' }] },
]);
check('cycle time starts at the first move', [times[0].cycle, times[0].lead], [4, 8]);
check('cycle time starts at the first move, in the dot', times[0].fromAt, { cycle: Date.parse('2026-01-05T09:00:00Z'), lead: Date.parse('2026-01-01T09:00:00Z') });
check('a task that never moved starts its cycle at creation', [times[1].cycle, times[1].lead], [8, 8]);
check('a move dated before creation starts the cycle at creation', [times[2].cycle, times[2].lead], [6, 6]);

var shellPage = { dataset: { boardId: 'abc' }, querySelector: function (sel) { return sel === '.bshell-report' ? {} : null; } };
currentDocument = pageWith({ '.board-settings-page[data-board-id]': shellPage });
check('a report page resolves its board for the modal', modalBoardId(), 'abc');
var plainShell = { dataset: { boardId: 'abc' }, querySelector: function () { return null; } };
currentDocument = pageWith({ '.board-settings-page[data-board-id]': plainShell });
check('a non-report shell page has no modal board', modalBoardId(), null);
currentDocument = pageWith({ '.board-wrap': { dataset: { boardId: 'xyz' } }, '.board-settings-page[data-board-id]': shellPage });
check('the board page keeps its own board id', modalBoardId(), 'xyz');

console.log('failures=' + failures);
if (failures) throw new Error(failures + ' report chart check(s) failed');
