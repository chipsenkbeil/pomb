var src = require('fs').readFileSync(process.argv[2], 'utf8');
function fn(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}

var TASK = { column_id: 'c2', swimlane_id: 'lane9' };
var calls = [];
var toasts = [];
var moveTargetColumnId = null;
var moveSwimlaneId = null;
var moveTaskId = null;
var modalDirty = false;

function option() { return { value: '', textContent: '' }; }
function select() {
  var s = { innerHTML: '', options: [], focus: function () {} };
  s.appendChild = function (o) { s.options.push(o); if (!s.value) s.value = o.value; };
  return s;
}
var elements = {
  'mt-title': { textContent: '' },
  'mt-board': select(),
  'mt-col': select(),
  'move-task-dialog': {
    hidden: true,
    classList: { toggle: function () {} },
    querySelector: function () { return { style: {} }; }
  },
  'modal-name': { value: 'Write docs' }
};
var document = {
  title: 'Board — Pomb',
  getElementById: function (id) { return elements[id] || null; },
  querySelector: function () { return null; },
  querySelectorAll: function () { return []; },
  createElement: option
};
var window = { location: {} };
function modalTaskId() { return 't1'; }
function modalBoardId() { return 'b1'; }
function cssEscape(s) { return s; }
function toast(m) { toasts.push(m); }
function closeModal() {}
function drained(res) { return res; }
var slow = false;
function fetchJson(url) {
  if (url === '/api/tasks/t1' && slow) return new Promise(function (r) { setTimeout(function () { r(TASK); }, 10); });
  if (url === '/api/tasks/t1') return Promise.resolve(TASK);
  if (url === '/api/boards/b1/columns') return Promise.resolve([{ id: 'c1', name: 'A' }, { id: 'c2', name: 'B' }]);
  return Promise.resolve([]);
}
function api(path, method, data) {
  calls.push({ path: path, method: method, data: data });
  return Promise.resolve({ ok: true });
}
eval(fn('  function modalTaskBoard() {'));
eval(fn('  function boardNameFor(id) {'));
eval(fn('  function copyModalTask() {'));
eval(fn('  function openMoveDialog(taskId, anchorCard, bulkIds) {'));
eval(fn('  function doMoveTask() {'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function tick() { return new Promise(function (resolve) { setTimeout(resolve, 0); }); }

copyModalTask();
tick().then(tick).then(function () {
  check('copy on a report page posts one task', calls.length, 1);
  check('copy goes to the task column and lane', calls[0] && calls[0].data, {
    column_id: 'c2', swimlane_id: 'lane9', name: 'Write docs (copy)', color_id: null
  });
  check('copy does not toast an error', toasts, ['Task copied.']);
  calls.length = 0;
  openMoveDialog();
  return tick().then(tick).then(tick);
}).then(function () {
  check('move keeps the task lane on a report page', moveSwimlaneId, 'lane9');
  check('move preselects the task column', moveTargetColumnId, 'c2');
  doMoveTask();
  check('move posts to the force endpoint', calls[0] && calls[0].path, '/api/tasks/t1/move?force=true');
  check('move posts the lane', calls[0] && calls[0].data.swimlane_id, 'lane9');
  calls.length = 0;
  toasts.length = 0;
  moveTargetColumnId = null;
  doMoveTask();
  check('move without a target posts nothing', calls.length, 0);
  check('move without a target says so', toasts, ['Could not move task.']);
  slow = true;
  moveSwimlaneId = null;
  openMoveDialog();
  return tick().then(tick).then(function () {
    elements['mt-board'].value = 'other';
    return new Promise(function (resolve) { setTimeout(resolve, 40); });
  });
}).then(function () {
  check('a late column load does not overwrite another board choice', elements['mt-col'].value, '');
  console.log('failures=' + failures);
  process.exit(failures ? 1 : 0);
}).catch(function (e) { console.log('ERROR ' + e.stack); process.exit(1); });
