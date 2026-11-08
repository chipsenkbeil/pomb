var src = require('fs').readFileSync(process.argv[2], 'utf8');
function fn(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}
var from = src.indexOf('  var DueDateDialog = {');
var DueDateDialog;
eval(src.slice(from, src.indexOf('\n  };\n', from) + '\n  };\n'.length));

function option() { return { value: '', textContent: '' }; }
var sel = {
  dataset: {},
  isConnected: true,
  options: [],
  _value: '',
  set innerHTML(v) { this.options = []; this._value = ''; },
  get innerHTML() { return ''; },
  appendChild: function (o) { this.options.push(o); },
  querySelector: function (q) {
    var m = /value="([^"]*)"/.exec(q);
    return this.options.filter(function (o) { return o.value === m[1]; })[0] || null;
  },
  set value(v) { this._value = v; },
  get value() { return this._value; },
};
var toasts = [];
var document = {
  getElementById: function () { return sel; },
  querySelector: function () { return null; },
  querySelectorAll: function () { return []; },
  createElement: option,
};
function cssEscape(s) { return s; }
function toast(m) { toasts.push(m); }
function modalBoardId() { return 'b1'; }
function fetchJson(url) {
  if (url === '/api/tasks/t1') return Promise.resolve({ column_id: 'c1' });
  return Promise.resolve([{ id: 'c1', name: 'A' }, { id: 'c2', name: 'B' }, { id: 'c3', name: 'Done' }]);
}

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function tick() { return new Promise(function (resolve) { setTimeout(resolve, 0); }); }

DueDateDialog.renderColumnList('t1', 'c3');
tick().then(tick).then(tick).then(function () {
  check('report page lists the columns after the task column',
    sel.options.map(function (o) { return o.value; }), ['', 'c2', 'c3']);
  check('report page keeps the stored target selected', sel.value, 'c3');
  check('no error toast', toasts, []);
  sel.options = [];
  DueDateDialog.renderColumnList('t1', 'c3');
  sel.dataset.taskId = 't2';
  return tick().then(tick).then(tick);
}).then(function () {
  check('a late column load for another task is dropped', sel.options.map(function (o) { return o.value; }), ['']);
  console.log('failures=' + failures);
  process.exit(failures ? 1 : 0);
}).catch(function (e) { console.log('ERROR ' + e.stack); process.exit(1); });
