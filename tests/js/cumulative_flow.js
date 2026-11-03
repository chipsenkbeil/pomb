// Cumulative flow bands follow columns by id through renames and
// deletes. Run with node against static/app.js; exits 1 on any
// failure:
//   node tests/js/cumulative_flow.js static/app.js
var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var CumulativeFlowPage = (function () {');
var end = src.indexOf('\n  })();', start) + '\n  })();'.length;
if (start < 0 || end < start) throw new Error('CumulativeFlowPage not found');
var document = {
  getElementById: function () { return null; },
  createElement: function () { return { style: {}, offsetWidth: 100, hidden: false }; }
};
function escapeHtml(s) { return String(s); }
var accountPrefs = { hour12: true, weekStart: 1 };
function inlineFn(start) {
  var from = src.indexOf(start);
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}
eval(inlineFn('  function daysIntoWeek(d) {'));
eval(inlineFn('  function clockText(d) {'));
eval(src.slice(src.indexOf('  var ReportChart = (function () {'), src.indexOf('\n  })();', src.indexOf('  var ReportChart = (function () {')) + '\n  })();'.length));
eval(src.slice(start, end));

function at(offset, h) {
  var d = new Date();
  d.setDate(d.getDate() + offset);
  d.setHours(h, 0, 0, 0);
  return d.toISOString();
}
function created(column, columnId, when) {
  var e = { kind: 'created', detail: 'Task created in ' + column, created_at: when };
  if (columnId) e.column_id = columnId;
  return e;
}
function moved(from, to, columnId, when) {
  var e = { kind: 'moved', detail: 'Moved from ' + from + ' to ' + to, created_at: when };
  if (columnId) e.column_id = columnId;
  return e;
}

var failures = 0;
function check(name, s, want) {
  var last = s.ends.length - 1;
  var band = function (label, back) {
    var k = s.order.indexOf(label);
    return k < 0 ? null : s.counts[k][last - back];
  };
  var problems = [];
  if (s.order.join('|') !== want.order) problems.push('bands ' + s.order.join('|'));
  (want.counts || []).forEach(function (c) {
    var got = band(c[0], c[1]);
    if (got !== c[2]) problems.push(c[0] + '@-' + c[1] + '=' + got + ' want ' + c[2]);
  });
  if (problems.length) failures++;
  console.log((problems.length ? 'FAIL ' : 'PASS ') + name +
    (problems.length ? ': ' + problems.join(', ') : ' | bands=' + s.order.join('|')));
}

var board = [{ id: 'todo', name: 'To-do', former_names: [] },
  { id: 'done', name: 'Done', former_names: [] }];

// Column Temp gets a task, is renamed Temp2, gets another, both leave,
// then the column is deleted: one band, under the newest name.
check('renamed then deleted column keeps one band', CumulativeFlowPage.seriesByDay({
  columns: board,
  tasks: [
    { id: 'a', column_id: 'todo', created_at: at(-3, 10), history: [
      created('Temp', 'temp', at(-3, 10)), moved('Temp2', 'To-do', 'todo', at(-1, 10))] },
    { id: 'b', column_id: 'done', created_at: at(-2, 10), history: [
      created('Temp2', 'temp', at(-2, 10)), moved('Temp2', 'Done', 'done', at(-1, 11))] }
  ]
}), { order: 'To-do|Done|Temp2',
  counts: [['Temp2', 3, 1], ['Temp2', 2, 2], ['Temp2', 1, 0], ['To-do', 0, 1], ['Done', 0, 1]] });

// A live column renamed after events keeps one band under its current
// name, with ids and through former_names for name-only history.
[true, false].forEach(function (withIds) {
  check('renamed live column, ' + (withIds ? 'column ids' : 'name-only history'),
    CumulativeFlowPage.seriesByDay({
      columns: [board[0], { id: 'rev', name: 'Reviewing', former_names: ['In Review'] }, board[1]],
      tasks: [{ id: 't', column_id: 'done', created_at: at(-1, 10), history: [
        created('In Review', withIds && 'rev', at(-1, 10)),
        moved('In Review', 'Done', withIds && 'done', at(0, 0))] }]
    }), { order: 'To-do|Reviewing|Done',
      counts: [['Reviewing', 1, 1], ['Reviewing', 0, 0], ['Done', 0, 1]] });
});

// Name-only history of a deleted column still gets a band by its name.
check('deleted column with name-only history', CumulativeFlowPage.seriesByDay({
  columns: board,
  tasks: [{ id: 'c', column_id: 'todo', created_at: at(-2, 10), history: [
    created('Old', null, at(-2, 10)), moved('Old', 'To-do', null, at(-1, 10))] }]
}), { order: 'To-do|Done|Old', counts: [['Old', 2, 1], ['To-do', 0, 1]] });

// Name-only history joins a deleted column's band when an id-bearing
// event recorded that name for it.
check('name-only event joins deleted column by recorded name', CumulativeFlowPage.seriesByDay({
  columns: board,
  tasks: [
    { id: 'd', column_id: 'todo', created_at: at(-3, 10), history: [
      created('Temp', null, at(-3, 10)), moved('Temp', 'To-do', null, at(-1, 10))] },
    { id: 'e', column_id: 'todo', created_at: at(-2, 10), history: [
      created('Temp', 'temp', at(-2, 10)), moved('Temp', 'To-do', 'todo', at(-1, 10))] }
  ]
}), { order: 'To-do|Done|Temp', counts: [['Temp', 2, 2], ['To-do', 0, 2]] });

// A deleted column that held nothing in the window leaves no band.
check('empty history-only band is dropped', CumulativeFlowPage.seriesByDay({
  columns: board,
  tasks: [{ id: 'f', column_id: 'todo', created_at: at(-60, 10), history: [
    created('Gone', 'gone', at(-60, 10)), moved('Gone', 'To-do', 'todo', at(-59, 10))] }]
}), { order: 'To-do|Done', counts: [['To-do', 0, 1]] });


function colors(html) {
  return (html.match(/chart-tip-swatch" style="background:[^"]+"/g) || []).length;
}

var flow = CumulativeFlowPage.seriesByDay({
  columns: board,
  tasks: [{ id: 'g', column_id: 'done', created_at: at(-2, 10), history: [
    created('To-do', 'todo', at(-2, 10)), moved('To-do', 'Done', 'done', at(-1, 10))] }]
});
var GEO = { W: 720, H: 320, padL: 46, padR: 32, padT: 16, padB: 34 };
var rect = { left: 100, top: 50, width: 360, height: 160 };
function at2x(px, py) { return [rect.left + px / 2, rect.top + py / 2]; }
function idx(px, py) { var p = at2x(px, py); return ReportChart.pointIndex(GEO, 30, rect, p[0], p[1]); }

function checkPlain(name, got, want) {
  if (JSON.stringify(got) !== JSON.stringify(want)) {
    failures++;
    console.log('FAIL ' + name + ': got ' + JSON.stringify(got) + ' want ' + JSON.stringify(want));
  }
}
checkPlain('pointer at the left edge of the plot', idx(46, 100), 0);
checkPlain('pointer at the right edge of the plot', idx(688, 100), 29);
checkPlain('pointer between two points snaps to the nearer', idx(46 + 642 / 29 * 10.4, 100), 10);
checkPlain('pointer left of the plot', idx(20, 100), -1);
checkPlain('pointer above the plot', idx(300, 4), -1);
checkPlain('pointer below the plot', idx(300, 300), -1);

var tip = CumulativeFlowPage.tipFor(flow, 28);
checkPlain('tooltip title is the long date', /^\d{1,2} [A-Z][a-z]+ 12:00 AM$/.test(tip.title), true);
checkPlain('tooltip has one row per column', tip.rows.map(function (r) { return r.label + '=' + r.value; }), ['To-do=0', 'Done=1']);
checkPlain('tooltip rows are coloured', colors(ReportChart.tipHtml(tip)), 2);

var svg = CumulativeFlowPage.chartSvg(flow);
checkPlain('chart draws a hidden crosshair', /<line class="chart-crosshair"[^>]*visibility="hidden"/.test(svg), true);

var handlers = {};
var attrs = {};
var line = { setAttribute: function (k, v) { attrs[k] = v; } };
var box = null;
var svgEl = {
  querySelector: function () { return line; },
  addEventListener: function (type, fn) { handlers[type] = fn; },
  getBoundingClientRect: function () { return rect; }
};
var host = {
  querySelector: function (sel) { return sel === 'svg' ? svgEl : box; },
  appendChild: function (el) { box = el; },
  getBoundingClientRect: function () { return { left: 90, top: 40, width: 400, height: 200 }; }
};
ReportChart.attachCrosshair(host, GEO, 30, function (i) { return CumulativeFlowPage.tipFor(flow, i); });
var p = at2x(46 + 642 / 29 * 10.4, 100);
handlers.pointermove({ clientX: p[0], clientY: p[1] });
checkPlain('crosshair follows the pointer to the nearest day', attrs.x1, ReportChart.pointX(GEO, 30, 10).toFixed(1));
checkPlain('crosshair is shown', attrs.visibility, 'visible');
checkPlain('tooltip is shown away from the data', box.hidden, false);
handlers.pointerleave();
checkPlain('leaving the plot hides the crosshair', attrs.visibility, 'hidden');
checkPlain('leaving the plot hides the tooltip', box.hidden, true);
p = at2x(10, 100);
handlers.pointermove({ clientX: p[0], clientY: p[1] });
checkPlain('pointer outside the plot shows nothing', box.hidden, true);
console.log('failures=' + failures);
if (failures) throw new Error(failures + ' cumulative flow check(s) failed');
