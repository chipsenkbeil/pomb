var src = require('fs').readFileSync(process.argv[2], 'utf8');
function fn(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}

var moveTargetColumnId = null;
var moveSwimlaneId = null;
var moveTaskId = null;
var moveBulkIds = null;
var toasts = [];
var modal = null;
var barTitle = null;
var boardsResult = null;

function select() {
  var s = { options: [], value: '', focus: function () {} };
  var html = '';
  Object.defineProperty(s, 'innerHTML', {
    get: function () { return html; },
    set: function (v) { html = v; s.options = []; s.value = ''; },
  });
  s.appendChild = function (o) { s.options.push(o); if (!s.value) s.value = o.value; };
  return s;
}
var boardSel = select();
var colSel = select();
var elements = {
  'mt-board': boardSel,
  'mt-col': colSel,
  'mt-title': { textContent: '' },
  'move-task-dialog': {
    hidden: true,
    classList: { toggle: function () {} },
    querySelector: function () { return { style: {} }; },
  },
};
var document = {
  title: 'Cycle & lead time | Pomb',
  getElementById: function (id) { return elements[id] || null; },
  querySelector: function (sel) {
    if (sel === '#modal-root .task-modal') return modal;
    if (sel === '.board-bar-title') return barTitle;
    return null;
  },
  querySelectorAll: function () { return []; },
  createElement: function () { return { value: '', textContent: '' }; },
};
var window = { location: {} };
var currentBoard = 'b1';
function modalTaskId() { return 't1'; }
function modalBoardId() { return currentBoard; }
function cssEscape(s) { return s; }
function toast(m) { toasts.push(m); }
function fetchJson(url) {
  if (url === '/api/tasks/t1') return Promise.resolve({ column_id: 'c2', swimlane_id: null });
  if (url === '/api/boards/b1/columns') return Promise.resolve([{ id: 'c1', name: 'A' }, { id: 'c2', name: 'B' }]);
  if (url === '/api/boards') return boardsResult();
  return Promise.resolve([]);
}
eval(fn('  function modalTaskBoard() {'));
eval(fn('  function boardNameFor(id) {'));
eval(fn('  function openMoveDialog(taskId, anchorCard, bulkIds) {'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function settle() {
  return new Promise(function (r) { setTimeout(r, 0); }).then(function () {
    return new Promise(function (r) { setTimeout(r, 0); });
  }).then(function () {
    return new Promise(function (r) { setTimeout(r, 0); });
  });
}
function labels(sel) { return sel.options.map(function (o) { return o.textContent; }); }

modal = { dataset: { boardId: 'b1', boardName: 'Alpha board' } };
boardsResult = function () { return Promise.reject(new Error('down')); };
openMoveDialog();
settle().then(function () {
  check('a report page labels the board with its name', labels(boardSel), ['Alpha board']);
  check('the label survives a failed boards call', boardSel.value, 'b1');
  check('the columns still load', colSel.options.length, 2);

  boardsResult = function () { return Promise.resolve([{ id: 'b2', name: 'Other' }]); };
  openMoveDialog();
  return settle();
}).then(function () {
  check('an archived board is kept when the list omits it', labels(boardSel), ['Alpha board', 'Other']);
  check('the selector stays on the current board', boardSel.value, 'b1');
  check('the late column load is kept for the current board', colSel.options.length, 2);
  check('the move targets the loaded column', moveTargetColumnId, 'c2');

  boardsResult = function () { return Promise.resolve([{ id: 'b1', name: 'Alpha board' }, { id: 'b2', name: 'Other' }]); };
  openMoveDialog();
  return settle();
}).then(function () {
  check('a listed board is not repeated', labels(boardSel), ['Alpha board', 'Other']);

  modal = null;
  barTitle = { textContent: ' Beta ' };
  boardsResult = function () { return Promise.reject(new Error('down')); };
  openMoveDialog();
  return settle();
}).then(function () {
  check('the board page reads its bar title', labels(boardSel), ['Beta']);
  console.log('failures=' + failures);
  process.exit(failures ? 1 : 0);
}).catch(function (e) { console.log('ERROR ' + e.stack); process.exit(1); });
