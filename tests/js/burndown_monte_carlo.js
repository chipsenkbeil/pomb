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
var accountPrefs = { hour12: true, weekStart: 1 };
function fn(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}
eval(fn('  function daysIntoWeek(d) {'));
eval(fn('  function clockText(d) {'));
eval(block('  var ReportChart = (function () {'));
eval(src.slice(src.indexOf('  function isoDate(d) {'), src.indexOf('\n  }\n', src.indexOf('  function isoDate(d) {')) + 4));
eval(block('  var ReportBreakdown = (function () {'));
eval(block('  var ReportRange = (function () {'));
eval(block('  var ReportValue = (function () {'));
eval(block('  var BurndownPage = (function () {'));
eval(block('  var MonteCarloPage = (function () {'));

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

var ends = [];
for (var j = 0; j < 30; j++) ends.push(new Date(2026, 9, 1 + j, 23, 59, 59, 999));
var counts = ends.map(function (_, k) { return 12 - Math.floor(k / 5); });

var flat = BurndownPage.chartSvg({ ends: ends, counts: counts });
check('burndown fills the area under the line', /<polygon[^>]*class="chart-area"/.test(flat), true);
check('burndown names the Y axis Task count', />Task count<\/text>/.test(flat), true);
check('burndown without a projection draws none', /chart-projection/.test(flat), false);

var series = BurndownPage.projectSeries({ ends: ends, counts: counts }, 2);
check('projection runs until the remaining tasks reach zero', series.projection, [5, 3, 1, 0]);
check('projection extends the dates one day each', series.ends.length, 34);
check('projection keeps the actual counts', series.counts.length, 30);
var proj = BurndownPage.chartSvg(series);
check('burndown draws the projection line', /<polyline[^>]*class="chart-projection"/.test(proj), true);
check('burndown labels the projected end date', ticks(proj).slice(-1)[0], 'Nov 3');
check('burndown tooltip on a projected day', BurndownPage.tipFor(series, 31),
  { title: '1 November 12:00 AM', rows: [{ label: 'Projected remaining', value: 3 }] });
check('burndown tooltip on an actual day', BurndownPage.tipFor(series, 3).rows[0].label, 'Tasks remaining');
check('no completions means no projection', BurndownPage.projectSeries({ ends: ends, counts: counts }, 0).projection, []);
check('nothing remaining means no projection',
  BurndownPage.projectSeries({ ends: ends, counts: counts.map(function () { return 0; }) }, 3).projection, []);
check('projection stops at a year', BurndownPage.projectSeries({ ends: ends, counts: [9000] }, 1).projection.length, 365);

check('Monte Carlo percentile lines', MonteCarloPage.PCTS, [95, 85, 70, 50]);
check('Monte Carlo line colours', MonteCarloPage.PCTS.map(function (p) { return MonteCarloPage.PCT_COLORS[p]; }),
  ['#1b5e20', '#7cb342', '#e6b800', '#fb8c00']);
check('Monte Carlo runs 10,000 simulations', MonteCarloPage.SIMULATIONS, 10000);

var today = new Date();
var midnight = new Date(today.getFullYear(), today.getMonth(), today.getDate()).getTime();
check('Today means midnight today', MonteCarloPage.startDateMs('text', 'Today'), midnight);
check('a picked date is used', MonteCarloPage.startDateMs('date', '2026-03-04'), new Date(2026, 2, 4).getTime());

var handlers = {};
var input = { type: 'text', value: 'Today', addEventListener: function (t, fn) { handlers[t] = fn; } };
MonteCarloPage.bindStartDate(input);
handlers.focus();
check('focusing the start date opens a date field', [input.type, /^\d{4}-\d{2}-\d{2}$/.test(input.value)], ['date', true]);
input.value = '2030-01-02';
handlers.blur();
check('a picked date stays after blur', [input.type, input.value], ['date', '2030-01-02']);
input.value = '';
handlers.blur();
check('an emptied start date returns to Today', [input.type, input.value], ['text', 'Today']);

var page = fs.readFileSync('templates/monte_carlo.html', 'utf8');
check('task count is a button that starts unset', /<button[^>]*id="mc-task-count"[^>]*>Set task count<\/button>/.test(page), true);
check('start date reads Today', /id="mc-start-date"[^>]*value="Today"/.test(page), true);
check('the simulations box is gone', /mc-sims/.test(page), false);
check('the Getting started box is in the page', /id="mc-getting-started"[^>]*>\s*<h3>Getting started/.test(page), true);
check('the charts wait for the first run', [/id="mc-basis-title" hidden/.test(page), /id="mc-chart-title" hidden/.test(page)], [true, true]);

console.log('failures=' + failures);
if (failures) throw new Error(failures + ' burndown and Monte Carlo check(s) failed');
