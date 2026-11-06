// The recycle bin dialog shows each task's short id, the first eight
// characters of its id. Run with node against static/app.js; exits 1 on
// any failure.
var fs = require('fs');
var src = fs.readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var BoardChrome = {');
var end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('BoardChrome not found');

var failures = 0;
function check(name, got, want) {
  var ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got)));
}

function element(tag) {
  return {
    tagName: tag,
    textContent: '',
    hidden: false,
    children: [],
    appendChild: function (child) { this.children.push(child); return child; },
    addEventListener: function () {},
    remove: function () {}
  };
}

var bin = {
  retention_days: 30,
  permanent_delete: false,
  tasks: [
    {
      id: '04a001fa-3c9d-4e2b-8f1a-6d7e5c3b2a10', name: 'Pay rent', location: 'Done',
      deleted_by: 'chip', deleted_at: '2026-11-01T10:00:00Z',
      unrestorable_after: '2026-12-01T10:00:00Z', restores_to: 'Doing'
    },
    {
      id: 'b7c2e9d0-1a2b-4c3d-9e8f-0a1b2c3d4e5f', name: 'File taxes', location: 'Todo',
      deleted_by: 'agent', deleted_at: '2026-11-02T10:00:00Z',
      unrestorable_after: '2026-12-02T10:00:00Z', restores_to: 'Todo'
    }
  ]
};

var rows = element('tbody');
var table = element('table');
table.hidden = true;
var note = element('p');
var elements = { 'bin-dialog': element('div'), 'bin-note': note, 'bin-table': table, 'bin-rows': rows };
var document = {
  body: { classList: { contains: function () { return false; }, toggle: function () {} } },
  getElementById: function (id) { return elements[id] || null; },
  createElement: function (tag) { return element(tag); }
};
var window = { location: { reload: function () {} } };
var fetch = function () {
  return Promise.resolve({ ok: true, json: function () { return Promise.resolve(bin); } });
};
var make = eval('(function (document, window, fetch) {' +
  'var boardId = function () { return "b1"; };' +
  'var accountPrefs = { hour12: false };' +
  'var showConfirmDialog = function () {}; var api = function () {}; var drained = function () {};' +
  'var toast = function () {};' +
  src.slice(start, end) + '\n return BoardChrome; })');
var chrome = make(document, window, fetch);

chrome.openBin();
setTimeout(function () {
  var cells = rows.children.map(function (tr) {
    return tr.children.map(function (td) { return td.textContent; });
  });
  check('recycle_bin_rows_should_start_with_the_short_task_id',
    cells.map(function (row) { return row[0]; }), ['04a001fa', 'b7c2e9d0']);
  check('recycle_bin_rows_should_keep_the_name_after_the_id',
    cells.map(function (row) { return row[1]; }), ['Pay rent', 'File taxes']);
  check('recycle_bin_table_should_show_once_it_has_rows', table.hidden, false);

  if (failures) process.exit(1);
  console.log('recycle bin ids ok');
}, 0);
