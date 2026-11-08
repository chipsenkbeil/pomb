var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var TimeInColumnPage = (function () {');
var end = src.indexOf('\n  })();', start) + '\n  })();'.length;
if (start < 0 || end < start) throw new Error('TimeInColumnPage not found');
var document = { getElementById: function () { return null; } };
function escapeHtml(s) { return String(s); }
var csvStart = src.indexOf('  function csvText(value) {');
var csvEnd = src.indexOf('  function downloadCsv(', csvStart);
if (csvStart < 0 || csvEnd < csvStart) throw new Error('csvText not found');
eval(src.slice(csvStart, csvEnd));
eval(src.slice(src.indexOf('  var ReportFile = (function () {'), src.indexOf('\n  })();', src.indexOf('  var ReportFile = (function () {')) + '\n  })();'.length));
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
  { id: 'done', name: 'Done', former_names: [] },
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
    ev('moved', 'Moved from In Review to Done', 'done', NOW - 4 * DAY - 3 * HOUR - 7 * MIN),
  ],
};
var b = {
  id: 'b', name: 'B', column_id: 'todo', created_at: iso(NOW - 2 * DAY),
  color_id: null, labels: [], member_ids: [], history: [],
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

var ta = TimeInColumnPage.taskRows(data, a, NOW);
check('task rows follow board order', ta.map(function (r) { return r.id; }), ['todo', 'rev', 'done']);
check('task row total', ta.map(function (r) { return r.ms; }), [4 * DAY, 2 * DAY - 3 * HOUR - 7 * MIN, 4 * DAY + 3 * HOUR + 7 * MIN]);
check('task row intervals', ta[1].intervals.map(function (iv) { return [iv.start, iv.end, iv.open]; }),
  [[NOW - 6 * DAY, NOW - 4 * DAY - 3 * HOUR - 7 * MIN, false]]);
check('current column interval is open and ends now', ta[2].intervals.map(function (iv) { return [iv.end, iv.open]; }), [[NOW, true]]);
check('task without moves', TimeInColumnPage.taskRows(data, b, NOW).map(function (r) { return [r.id, r.ms]; }), [['todo', 2 * DAY]]);

var ms = 27 * DAY + 3 * HOUR + 7 * MIN + 40000;
check('minutes', TimeInColumnPage.fmtDuration(ms, 'minutes'), '27d 03h 08m');
check('hours', TimeInColumnPage.fmtDuration(ms, 'hours'), '27d 03h');
check('days', TimeInColumnPage.fmtDuration(ms, 'days'), '27d');
check('zero minutes', TimeInColumnPage.fmtDuration(0, 'minutes'), '0m');

var board = TimeInColumnPage.buildTable(data, all, NOW);
check('board human export', TimeInColumnPage.boardExportRows(board, 'human', 'minutes', NOW),
  [['Task', 'To-do', 'Reviewing', 'Done'], ['A', '4d 00h 00m', '1d 20h 53m', '4d 03h 07m'], ['B', '2d 00h 00m', '0m', '0m']]);
check('board seconds export', TimeInColumnPage.boardExportRows(board, 'seconds', 'minutes', NOW).slice(1),
  [['A', 4 * DAY / 1000, (2 * DAY - 3 * HOUR - 7 * MIN) / 1000, (4 * DAY + 3 * HOUR + 7 * MIN) / 1000], ['B', 2 * DAY / 1000, 0, 0]]);
check('board periods export', TimeInColumnPage.boardExportRows(board, 'periods', 'minutes', NOW)[2],
  ['B', iso(NOW - 2 * DAY) + ' - ' + iso(NOW), '', '']);
var filtered = TimeInColumnPage.buildTable(data, opts({ label: 'bug', columns: ['rev'] }), NOW);
check('export honours filters', TimeInColumnPage.boardExportRows(filtered, 'seconds', 'minutes', NOW),
  [['Task', 'Reviewing'], ['A', (2 * DAY - 3 * HOUR - 7 * MIN) / 1000]]);
check('task export', TimeInColumnPage.taskExportRows(ta, 'seconds', NOW).slice(0, 2),
  [['Column', 'Time total seconds'], ['To-do', 4 * DAY / 1000]]);
check('task periods export', TimeInColumnPage.taskExportRows(ta, 'periods', NOW)[3],
  ['Done', iso(NOW - 4 * DAY - 3 * HOUR - 7 * MIN) + ' - ' + iso(NOW)]);

var csv = TimeInColumnPage.exportFile([['Task', 'a,"b"'], ['=cmd', 5]], 'csv', 'x');
check('csv name', csv.name, 'x.csv');
check('csv text', Buffer.from(csv.bytes).toString('utf8'), '﻿Task,"a,""b"""\r\n\'=cmd,5');

function crc32(buf) {
  var c = 0xFFFFFFFF;
  for (var i = 0; i < buf.length; i++) {
    c ^= buf[i];
    for (var k = 0; k < 8; k++) c = c & 1 ? 0xEDB88320 ^ (c >>> 1) : c >>> 1;
  }
  return (c ^ 0xFFFFFFFF) >>> 0;
}

function readZip(bytes) {
  var buf = Buffer.from(bytes);
  var eocd = buf.length - 22;
  if (buf.readUInt32LE(eocd) !== 0x06054b50) throw new Error('no end of central directory');
  var count = buf.readUInt16LE(eocd + 10);
  var at = buf.readUInt32LE(eocd + 16);
  var files = {};
  for (var i = 0; i < count; i++) {
    if (buf.readUInt32LE(at) !== 0x02014b50) throw new Error('bad central entry');
    var crc = buf.readUInt32LE(at + 16), size = buf.readUInt32LE(at + 24);
    var nameLen = buf.readUInt16LE(at + 28), local = buf.readUInt32LE(at + 42);
    var name = buf.slice(at + 46, at + 46 + nameLen).toString('utf8');
    if (buf.readUInt32LE(local) !== 0x04034b50) throw new Error('bad local header');
    var start = local + 30 + buf.readUInt16LE(local + 26) + buf.readUInt16LE(local + 28);
    var data = buf.slice(start, start + size);
    if (crc32(data) !== crc) throw new Error('crc mismatch ' + name);
    files[name] = data.toString('utf8');
    at += 46 + nameLen;
  }
  return files;
}
var x = TimeInColumnPage.exportFile([['Task', 'T<&>"1'], ['=cmd', 90]], 'xlsx', 'x');
check('xlsx name', x.name, 'x.xlsx');
var z = readZip(x.bytes);
check('xlsx parts', Object.keys(z).sort(),
  ['[Content_Types].xml', '_rels/.rels', 'xl/_rels/workbook.xml.rels', 'xl/workbook.xml', 'xl/worksheets/sheet1.xml']);
check('xlsx cells', z['xl/worksheets/sheet1.xml'].replace(/^.*<sheetData>|<\/sheetData>.*$/g, ''),
  '<row r="1"><c r="A1" t="inlineStr"><is><t xml:space="preserve">Task</t></is></c><c r="B1" t="inlineStr"><is><t xml:space="preserve">T&lt;&amp;&gt;&quot;1</t></is></c></row>' +
  '<row r="2"><c r="A2" t="inlineStr"><is><t xml:space="preserve">=cmd</t></is></c><c r="B2"><v>90</v></c></row>');

var big = readZip(TimeInColumnPage.exportFile([['x'.repeat(40000)]], 'xlsx', 'x').bytes);
check('xlsx cell is capped', big['xl/worksheets/sheet1.xml'].match(/<t [^>]*>(x+ \.\.\.)<\/t>/)[1].length, 32767);

check('option lists', [TimeInColumnPage.exportKindOptions(), TimeInColumnPage.exportFormatOptions()],
  ['<option value="human">Time human readable</option><option value="seconds">Time total seconds</option><option value="periods">Periods</option>',
    '<option value="xlsx">Excel</option><option value="csv">CSV</option>']);


var ticHost = { innerHTML: '' };
document.getElementById = function (id) { return id === 'tic-table' ? ticHost : null; };
TimeInColumnPage.render(t, 'minutes');
check('table rows carry their task id', (ticHost.innerHTML.match(/<tr class="tic-row" tabindex="0" data-task-id="[^"]*"/g) || []).length, 2);
check('table rows name the task they open', ticHost.innerHTML.indexOf('data-task-id="a"><th scope="row">A</th>') !== -1, true);

var listeners = {};
var opened = [];
TimeInColumnPage.attachRows({ addEventListener: function (type, fn) { listeners[type] = fn; } },
  function (id) { opened.push(id); });
var rowEl = { dataset: { taskId: 'a' } };
function rowEvent(row) {
  return { key: 'Enter', preventDefault: function () {}, target: { closest: function () { return row; } } };
}
listeners.click(rowEvent(rowEl));
listeners.keydown(rowEvent(rowEl));
listeners.click(rowEvent(null));
check('clicking a task row, or Enter on it, opens that task', opened, ['a', 'a']);
console.log('failures=' + failures);
if (failures) throw new Error(failures + ' time in column check(s) failed');
