var src = require('fs').readFileSync(process.argv[2], 'utf8');
function fn(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}

var TASK = {
  name: 'Write docs', description: 'Body', column_id: 'c2', swimlane_id: 'lane9',
  color_id: 'col3', labels: ['a'], timeline_start: '2026-10-01', timeline_end: '2026-10-05',
  links: [{ target_id: 't9' }], attachments: [{ id: 'f1' }],
};
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
  Object.defineProperty(s, 'selectedIndex', { get: function () {
    return s.options.findIndex(function (o) { return o.value === s.value; });
  } });
  return s;
}
var elements = {
  'mt-title': { textContent: '' },
  'mt-board': select(),
  'mt-col': select(),
  'move-task-dialog': {
    hidden: true,
    classList: { toggle: function () {} },
    querySelector: function () { return { style: {} }; },
  },
  'modal-name': { value: 'Write docs' },
  'cp-name': { value: '', focus: function () {}, select: function () {} },
  'cp-col': select(),
  'cp-copy-btn': { disabled: false },
  'copy-task-dialog': { hidden: true },
};
var document = {
  title: 'Board — Pomb',
  getElementById: function (id) { return elements[id] || null; },
  querySelector: function () { return null; },
  querySelectorAll: function () { return []; },
  createElement: option,
};
var stored = {};
var window = {
  location: {},
  sessionStorage: {
    setItem: function (k, v) { stored[k] = v; },
    getItem: function (k) { return stored[k] || null; },
    removeItem: function (k) { delete stored[k]; },
  },
};
function modalTaskId() { return 't1'; }
function modalBoardId() { return 'b1'; }
function cssEscape(s) { return s; }
function toast(m) { toasts.push(m); }
function closeModal() {}
function drained(res) { return Promise.resolve(res); }
function toastRefusal(res, title) { toasts.push(title); }
var slow = false;
var releaseTask = null;
function fetchJson(url) {
  if (url === '/api/tasks/t1' && slow) return new Promise(function (r) { releaseTask = function () { r(TASK); }; });
  if (url === '/api/tasks/t1') return Promise.resolve(TASK);
  if (url === '/api/boards/b1/columns') return Promise.resolve([{ id: 'c1', name: 'A' }, { id: 'c2', name: 'B' }]);
  return Promise.resolve([]);
}
function api(path, method, data) {
  calls.push({ path: path, method: method, data: data });
  return Promise.resolve({ ok: true });
}
const withForce = (body, force) => (force ? Object.assign({}, body, { force: true }) : body);
const withWip = (send) => send(false);
const moveWithWip = (id, body) => api('/api/tasks/' + id + '/move', 'POST', body);
eval(fn('  function modalTaskBoard() {'));
eval(fn('  function boardNameFor(id) {'));
eval(fn('  function openCopyDialog(taskId) {'));
eval(fn('  function doCopyTask() {'));
var copySource = null;
eval(fn('  function openMoveDialog(taskId, anchorCard, bulkIds) {'));
eval(fn('  function doMoveTask() {'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function tick() { return new Promise(function (resolve) { setTimeout(resolve, 0); }); }

openCopyDialog();
tick().then(tick).then(tick).then(function () {
  check('the dialog opens', elements['copy-task-dialog'].hidden, false);
  check('the name is prefilled', elements['cp-name'].value, 'Write docs (copy)');
  check('the task column is preselected', elements['cp-col'].value, 'c2');
  elements['cp-name'].value = ' Renamed ';
  elements['cp-col'].value = 'c1';
  doCopyTask();
  doCopyTask();
  return tick().then(tick).then(tick);
}).then(function () {
  check('copy posts one task even when submitted twice', calls.length, 1);
  check('the copy button is usable again', elements['cp-copy-btn'].disabled, false);
  check('copy goes to the chosen column with the typed name and the kept fields', calls[0] && calls[0].data, {
    column_id: 'c1', swimlane_id: 'lane9', name: 'Renamed', description: 'Body',
    color_id: 'col3', labels: ['a'], timeline_start: '2026-10-01', timeline_end: '2026-10-05',
  });
  check('copy names the target column', toasts, ['Task was copied to column A.']);
  check('the dialog closes', elements['copy-task-dialog'].hidden, true);
  toasts.length = 0;
  elements['cp-name'].value = '  ';
  calls.length = 0;
  doCopyTask();
  check('an empty name posts nothing', calls.length, 0);
  calls.length = 0;
  openMoveDialog();
  return tick().then(tick).then(tick);
}).then(function () {
  check('move keeps the task lane on a report page', moveSwimlaneId, 'lane9');
  check('move preselects the task column', moveTargetColumnId, 'c2');
  doMoveTask();
  check('move asks first instead of forcing', calls[0] && calls[0].path, '/api/tasks/t1/move');
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
    releaseTask();
    return tick().then(tick).then(tick);
  });
}).then(function () {
  check('a late column load does not overwrite another board choice', elements['mt-col'].value, '');
  console.log('failures=' + failures);
  process.exit(failures ? 1 : 0);
}).catch(function (e) { console.log('ERROR ' + e.stack); process.exit(1); });
