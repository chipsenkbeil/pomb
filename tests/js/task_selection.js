var src = require('fs').readFileSync(process.argv[2], 'utf8');
function slice(from, to) {
  var start = src.indexOf(from);
  var end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
}

var dragging = false;
var toasts = [];
var requests = [];
var confirms = [];
var focused = [];
var blocked = null;
var cards = [];
var apiOk = true;
function toast(msg) { toasts.push(msg); }
function cssEscape(s) { return s; }
function boardId() { return 'b1'; }
function drained(res) { return Promise.resolve(res); }
function api(path, method, data) {
  requests.push([method, path, data]);
  return Promise.resolve({ ok: apiOk });
}
function showConfirmDialog(title, message, ok, onConfirm) { confirms.push([title, message, onConfirm]); }
var memberIdsOf = {};
var searchBox = { value: 'x', focus: function () {} };
var membersList = { innerHTML: '' };
var membersDialog = { hidden: true };
var refreshed = [];
var rendered = [];
var TimerUI = { refreshTaskCard: function (id) { refreshed.push(id); } };
var membersBulkIds = null;
var membersDialogTaskId = 'old';
var membersRoster = [];
var membersAssigned = [];
var membersResponsible = 'old';
function initMembersDialog() {}
function renderMembersList(filter) { rendered.push(filter); }
function fetchJson() { return Promise.resolve([{ id: 'u1', username: 'me' }]); }
var document = {
  querySelectorAll: function (sel) { return sel === '.task-card' ? cards.map(function (c) { return c.el; }) : []; },
  querySelector: function (sel) {
    var m = /data-task-id="([^"]*)"/.exec(sel);
    if (!m) return blocked;
    var hit = cards.filter(function (c) { return c.id === m[1]; })[0];
    if (hit) hit.el.dataset.memberIds = memberIdsOf[m[1]] || '';
    return hit ? hit.el : null;
  },
  getElementById: function (id) {
    return id === 'members-search' ? searchBox : id === 'members-list' ? membersList : membersDialog;
  },
  addEventListener: function () {}
};
var window = { location: { reload: function () { requests.push(['reload']); } } };
eval(slice('  var selectedIds = [];', '  function initSelection'));

function card(id, col, lane, index) {
  var classes = {};
  var el = {
    hidden: false,
    offsetParent: {},
    dataset: { taskId: id, taskName: 'Task ' + id },
    classList: { toggle: function (name, on) { classes[name] = on; } },
    scrollIntoView: function () {},
    focus: function () { focused.push(id); },
    closest: function (sel) { return sel === '.task-card' ? el : null; }
  };
  var entry = { id: id, el: el, col: col, lane: lane, index: index, classes: classes };
  cards.push(entry);
  return entry;
}
var a1 = card('a1', 0, 'L', 0), a2 = card('a2', 0, 'L', 1), a3 = card('a3', 0, 'L', 2);
var b1 = card('b1', 1, 'L', 0), b2 = card('b2', 1, 'L', 1);
var d1 = card('d1', 3, 'L', 0);
var x1 = card('x1', 0, 'M', 0);
boardCardOrder = function () { return cards; };

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function click(entry, mods) {
  var event = { target: entry.el, metaKey: false, ctrlKey: false, shiftKey: false, prevented: false, stopped: false,
    preventDefault: function () { this.prevented = true; }, stopPropagation: function () { this.stopped = true; } };
  Object.keys(mods || {}).forEach(function (k) { event[k] = mods[k]; });
  onSelectionClick(event);
  return event;
}
function key(k, extra) {
  var event = { key: k, target: { tagName: 'BODY' }, altKey: false, ctrlKey: false, metaKey: false, shiftKey: false,
    prevented: false, preventDefault: function () { this.prevented = true; } };
  Object.keys(extra || {}).forEach(function (name) { event[name] = extra[name]; });
  onSelectionKey(event);
  return event;
}

var plain = click(a1);
check('a plain click leaves the card to the opener', [plain.prevented, plain.stopped, selectedIds], [false, false, []]);

var ctrl = click(a1, { ctrlKey: true });
check('ctrl+click selects the card', selectedIds, ['a1']);
check('ctrl+click does not reach the card opener', [ctrl.prevented, ctrl.stopped], [true, true]);
check('ctrl+click paints the card selected', a1.classes['task-card--selected'], true);
click(a3, { metaKey: true });
check('cmd+click adds a card', selectedIds, ['a1', 'a3']);
click(a1, { ctrlKey: true });
check('ctrl+click on a selected card drops it', selectedIds, ['a3']);
check('dropping a card repaints it plain', a1.classes['task-card--selected'], false);

clearSelection();
click(a2, { ctrlKey: true });
click(b2, { shiftKey: true });
check('shift+click selects from the last click across columns', selectedIds, ['a2', 'a3', 'b1', 'b2']);
click(a1, { shiftKey: true });
check('shift+click backwards replaces the range', selectedIds, ['a1', 'a2']);
clearSelection();
click(d1, { shiftKey: true });
check('shift+click with no earlier click selects just that card', selectedIds, ['d1']);

check('dragging an unselected card is a plain drag', dragGroupIds(a1.el), null);
check('dragging the only selected card is a plain drag', dragGroupIds(d1.el), null);
setSelection(['b1', 'a2', 'd1'], 'a2');
check('dragging a selected card takes the rest in board order', dragGroupIds(d1.el), ['d1', 'a2', 'b1']);
requests.length = 0;
moveSelectionTo(['d1', 'a2'], { dataset: { columnId: 'c9', swimlaneId: 'L' } }, 2);
check('a group drop posts one bulk move', requests[0], ['POST', '/api/boards/b1/tasks/bulk',
  { task_ids: ['d1', 'a2'], action: 'move', column_id: 'c9', swimlane_id: 'L', position: 2 }]);

setSelection(['a2'], 'a2');
var down = key('ArrowDown');
check('ArrowDown replaces the selection with the next card', selectedIds, ['a3']);
check('an arrow key is consumed', down.prevented, true);
key('ArrowDown');
check('ArrowDown at the end of a cell stays', selectedIds, ['a3']);
key('ArrowRight');
check('ArrowRight takes the card at the clamped place in the next column', selectedIds, ['b2']);
key('ArrowRight');
check('ArrowRight skips a column with no cards in the lane', selectedIds, ['d1']);
key('ArrowRight');
check('ArrowRight at the last column stays', selectedIds, ['d1']);
key('ArrowLeft');
check('ArrowLeft goes back to the nearest column with cards', selectedIds, ['b1']);
key('ArrowUp');
check('ArrowUp at the top of a cell stays', selectedIds, ['b1']);
check('an arrow key focuses the card it reaches', focused[focused.length - 1], 'b1');

setSelection(['a1', 'a2'], 'a1');
key('ArrowDown', { shiftKey: true });
check('Shift+Arrow leaves the selection alone', selectedIds, ['a1', 'a2']);
key('Escape');
check('Escape clears the selection', selectedIds, []);
check('Escape unpaints the cards', a1.classes['task-card--selected'], false);

setSelection(['a1'], 'a1');
key('Escape', { target: { tagName: 'INPUT' } });
check('Escape in a text field keeps the selection', selectedIds, ['a1']);
blocked = {};
key('Escape');
check('Escape under a dialog keeps the selection', selectedIds, ['a1']);
blocked = null;

setSelection(['a1', 'b1'], 'a1');
var del = key('Delete');
check('Delete asks first', [confirms.length, confirms[0][0], confirms[0][1]], [1, 'Delete tasks', 'Delete 2 tasks?']);
check('Delete is consumed', del.prevented, true);
requests.length = 0;
confirms[0][2]();
check('confirming deletes each selected task', requests.slice(0, 2), [['DELETE', '/api/tasks/a1', undefined], ['DELETE', '/api/tasks/b1', undefined]]);
confirms.length = 0;
key('Delete', { shiftKey: true });
check('Shift+Delete asks the same question', confirms.length, 1);

setSelection(['a1'], 'a1');
confirms.length = 0;
key('Delete');
check('Delete of one task names it', confirms[0][1], 'Delete "Task a1"?');

setSelection([], null);
confirms.length = 0;
key('Delete');
check('Delete with no selection asks nothing', confirms.length, 0);

check('no other function shares the reload helper name', src.split('function reloadSelectionSoon(').length, 2);
check('the selection is wired to the board', /initTaskSortable\(\);\n\s+initSelection\(\);/.test(src), true);
check('a drop of a selected card posts the group', /var group = dragGroupIds\(card\);/.test(src), true);

var pending = [];
function flush() { return new Promise(function (resolve) { setTimeout(resolve, 0); }); }

requests.length = 0;
pending.push(bulkColor(['a1', 'b1'], 'c-red').then(function () {
  check('bulk color posts one request for the selection', requests.filter(function (r) { return r[0] !== 'reload'; }), [['POST', '/api/boards/b1/tasks/bulk',
    { task_ids: ['a1', 'b1'], action: 'color', color_id: 'c-red' }]]);
  check('bulk color refreshes every card', refreshed, ['a1', 'b1']);
}));

pending.push(flush().then(function () {
  requests.length = 0; refreshed.length = 0;
  return bulkUnassign(['a2', 'a3']);
}).then(function () {
  check('unassign all posts one request', requests, [['POST', '/api/boards/b1/tasks/bulk',
    { task_ids: ['a2', 'a3'], action: 'unassign_members' }]]);
  check('unassign all refreshes the cards', refreshed, ['a2', 'a3']);
  memberIdsOf = { a1: 'u1,u2', b1: 'u1' };
  check('the dialog starts with the members every task has', commonMembers(['a1', 'b1']), ['u1']);
  check('no common members when one task has none', commonMembers(['a1', 'a2']), []);
  openBulkMembersDialog(['a1', 'b1']);
  check('the dialog opens in bulk mode', [membersBulkIds, membersDialogTaskId, membersAssigned, membersDialog.hidden],
    [['a1', 'b1'], null, ['u1'], false]);
  return flush();
}).then(function () {
  check('the dialog lists the roster', membersRoster, [{ id: 'u1', username: 'me' }]);
  requests.length = 0; refreshed.length = 0;
  toggleBulkMember('u1');
  return flush();
}).then(function () {
  check('a member every task has is removed from all', requests[0][2],
    { task_ids: ['a1', 'b1'], action: 'members', add_member_ids: [], remove_member_ids: ['u1'] });
  check('the dialog follows the removal', membersAssigned, []);
  requests.length = 0;
  toggleBulkMember('u1');
  return flush();
}).then(function () {
  check('a member not on every task is added to all', requests[0][2],
    { task_ids: ['a1', 'b1'], action: 'members', add_member_ids: ['u1'], remove_member_ids: [] });
  check('the dialog follows the addition', membersAssigned, ['u1']);
  check('the cards refresh after a member change', refreshed.slice(-2), ['a1', 'b1']);
}));

Promise.all(pending).then(function () {
  check('the context menu opens the bulk menu for a selected card', /isSelected\(card\) && selectedCards\(\)\.length > 1\) openBulkMenu/.test(src), true);
  apiOk = false;
  toasts.length = 0;
  requests.length = 0;
  moveSelectionTo(['d1', 'a2'], { dataset: { columnId: 'c9' } }, 0);
  setTimeout(function () {
    check('a failed bulk move shows a toast', toasts, ['Could not move the selected tasks.']);
    check('a failed bulk move does not reload at once', requests.filter(function (r) { return r[0] === 'reload'; }).length, 0);
    if (failures) process.exit(1);
    console.log('task_selection bulk ok');
    process.exit(0);
  }, 0);
});

setTimeout(function () {
  if (failures) process.exit(1);
  console.log('task_selection ok');
}, 0);
