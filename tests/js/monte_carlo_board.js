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
eval(block('  var ReportChart = (function () {'));
eval(src.slice(src.indexOf('  function isoDate(d) {'), src.indexOf('\n  }\n', src.indexOf('  function isoDate(d) {')) + 4));
eval(block('  var ReportBreakdown = (function () {'));
eval(block('  var ReportFile = (function () {'));
eval(block('  var MonteCarloPage = (function () {'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

var tasks = [
  { id: 'a', column_id: 'todo', member_ids: ['u1'], color_id: 'red', labels: ['bug'] },
  { id: 'b', column_id: 'todo', member_ids: [], color_id: null, labels: [] },
  { id: 'c', column_id: 'done', member_ids: ['u1'], color_id: 'red', labels: ['bug', 'ui'], completed_at: '2026-01-01T00:00:00Z' },
  { id: 'd', column_id: 'doing', member_ids: ['u2'], color_id: 'blue', labels: ['ui'] }
];
check('board count skips completed tasks by default', MonteCarloPage.boardCount(tasks, { openOnly: true }), 3);
check('board count by column', MonteCarloPage.boardCount(tasks, { column: 'done', openOnly: true }), 1);
check('board count by user', MonteCarloPage.boardCount(tasks, { openOnly: true, user: 'u1' }), 1);
check('board count of unassigned tasks', MonteCarloPage.boardCount(tasks, { openOnly: true, user: 'none' }), 1);
check('board count by colour', MonteCarloPage.boardCount(tasks, { color: 'red' }), 2);
check('board count of tasks with no colour', MonteCarloPage.boardCount(tasks, { color: 'none' }), 1);
check('board count by label', MonteCarloPage.boardCount(tasks, { label: 'ui' }), 2);
check('board count combines the filters', MonteCarloPage.boardCount(tasks, { user: 'u1', color: 'red', label: 'ui' }), 1);

var now = new Date(2026, 10, 15, 13, 0);
function day(ms) { var d = new Date(ms); return [d.getFullYear(), d.getMonth() + 1, d.getDate()]; }
var r30 = MonteCarloPage.periodRange('30', '', '', now);
check('30 days spans 30 days ending today', [r30.days, day(r30.start)], [30, [2026, 10, 17]]);
var r6 = MonteCarloPage.periodRange('6m', '', '', now);
check('6 months starts six months back', day(r6.start), [2026, 5, 15]);
var r12 = MonteCarloPage.periodRange('12m', '', '', now);
check('12 months starts a year back', day(r12.start), [2025, 11, 15]);
var rc = MonteCarloPage.periodRange('custom', '2026-10-01', '2026-10-10', now);
check('custom range counts both ends', [rc.days, day(rc.start)], [10, [2026, 10, 1]]);
check('custom range with the end before the start is refused', MonteCarloPage.periodRange('custom', '2026-10-10', '2026-10-01', now), null);
check('custom range without dates is refused', MonteCarloPage.periodRange('custom', '', '', now), null);

var end31 = MonteCarloPage.periodRange('6m', '', '', new Date(2026, 7, 31));
check('6 months before 31 August is 28 February', day(end31.start), [2026, 2, 28]);
var future = MonteCarloPage.periodRange('custom', '2026-11-10', '2027-01-01', now);
check('a custom range ends today at the latest', future.days, 6);
var dst = MonteCarloPage.dailyThroughput([], 'Done', new Date(2026, 9, 30).getTime(), 6);
check('days across a clock change all start at midnight', dst.starts.map(function (d) { return [d.getMonth() + 1, d.getDate(), d.getHours()]; }),
  [[10, 30, 0], [10, 31, 0], [11, 1, 0], [11, 2, 0], [11, 3, 0], [11, 4, 0]]);

var moved = { id: 'm', history: [{ kind: 'moved', detail: 'Moved from To do to Done', created_at: new Date(2026, 9, 2, 10).toISOString() }] };
var th = MonteCarloPage.dailyThroughput([moved], 'Done', new Date(2026, 9, 1).getTime(), 5);
check('throughput counts arrivals inside the range', th.counts, [0, 1, 0, 0, 0]);

var fc = { nTasks: 12, startMs: new Date(2026, 10, 15).getTime(), sims: 10000, basisTotal: 40,
  sorted: [new Date(2026, 11, 1).getTime(), new Date(2026, 11, 2).getTime(), new Date(2026, 11, 3).getTime()] };
var rows = MonteCarloPage.forecastRows(fc);
check('forecast rows start with the percentiles', rows.slice(0, 5).map(function (r) { return r[0]; }), ['Probability', '95%', '85%', '70%', '50%']);
check('forecast rows carry the settings', rows.slice(-4), [['Tasks', 12], ['Start date', '2026-11-15'], ['Simulations', 10000], ['Completed tasks in the period', 40]]);
check('forecast text lists the 95 percent date', MonteCarloPage.forecastLines(fc)[2], '95% probability: December 3, 2026');
check('not enough data names the minimum', /at least 5 completed tasks/.test(MonteCarloPage.notEnoughMessage(3)), true);

function ascii(bytes) { return Array.prototype.map.call(bytes, function (b) { return String.fromCharCode(b); }).join(''); }
var pdf = ascii(ReportFile.pdfBytes('Title (x)', MonteCarloPage.forecastLines(fc)));
check('pdf header and trailer', [pdf.indexOf('%PDF-1.4'), /%%EOF\n$/.test(pdf)], [0, true]);
var startxref = +/startxref\n(\d+)/.exec(pdf)[1];
check('pdf startxref points at the xref table', pdf.slice(startxref, startxref + 4), 'xref');
var offsets = pdf.slice(startxref).split('\n').filter(function (l) { return / 00000 n $/.test(l); }).map(function (l) { return +l.slice(0, 10); });
check('pdf xref offsets point at objects', offsets.map(function (o, i) { return pdf.slice(o).indexOf((i + 1) + ' 0 obj') === 0; }).every(Boolean), true);
check('pdf escapes parentheses in text', pdf.indexOf('(Title \\(x\\))') > 0, true);
var many = ascii(ReportFile.pdfBytes('T', new Array(100).fill('line')));
check('pdf pages long text', /\/Count 3 /.test(many), true);
var xlsx = ReportFile.xlsxBytes(rows, 'Forecast');
check('xlsx is a zip', [xlsx[0], xlsx[1]], [0x50, 0x4b]);
check('xlsx names the sheet', ascii(xlsx).indexOf('<sheet name="Forecast"') > 0, true);

var page = fs.readFileSync('templates/monte_carlo.html', 'utf8');
check('task count opens a popover with both modes',
  [/id="mc-count-mode-fixed"/.test(page), /Count tasks on board \(current count: <span id="mc-count-current">/.test(page)], [true, true]);
check('popover filters', ['mc-count-column', 'mc-count-user', 'mc-count-color', 'mc-count-label'].every(function (id) { return page.indexOf('id="' + id + '"') > 0; }), true);
check('more options reveal user, colour and label', ['mc-user', 'mc-color', 'mc-label'].every(function (id) { return page.indexOf('id="' + id + '"') > 0; }), true);
check('period has six choices', (page.match(/<option value="(30|60|90|6m|12m|custom)"/g) || []).length, 6);
check('export buttons', [/id="mc-export-pdf"/.test(page), /id="mc-export-xlsx"/.test(page)], [true, true]);

console.log('failures=' + failures);
if (failures) throw new Error(failures + ' Monte Carlo board check(s) failed');
