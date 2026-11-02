var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var Notifications = {');
var end = src.indexOf('  function initNotificationsPanel() {', start);
if (start < 0 || end < start) throw new Error('notifications object not found');

var synced = 0;
var TimerUI = { syncBell: function () { synced++; } };
var replies = {};
function jsonIfJson(res) { return res; }
function fetch(url) { return Promise.resolve(replies[url]); }
var Notifications = eval('(' + src.slice(start, end).trim().replace(/^var Notifications = /, '').replace(/;\s*$/, '') + ')');

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

check('no unread notifications keep the break badge text', Notifications.badgeText(), '1');
Notifications.setUnread(3);
check('the badge shows the unread count', Notifications.badgeText(), '3');
check('setting the count syncs the bell', synced, 1);
Notifications.setUnread(250);
check('the badge caps the count', Notifications.badgeText(), '99+');

var moved = { kind: 'task_moved', actor: 'api-token', token_name: 'ci', task_name: 'Ship', detail: 'Done', board_id: 'b1', task_id: 't1', board_name: 'Work' };
check('the actor is the token name', Notifications.actorName(moved), 'ci');
check('a move names its column', Notifications.describe(moved), 'moved Ship to Done');
check('a task event links to the task', Notifications.linkFor(moved), { href: '/b/b1#task-t1', label: 'VIEW TASK' });
check('a column event links to the board', Notifications.linkFor({ kind: 'column_deleted', board_id: 'b1', task_id: null }), { href: '/b/b1', label: 'VIEW BOARD' });
check('a deleted task has no link', Notifications.linkFor({ kind: 'task_deleted', board_id: 'b1', task_id: 't1' }), null);
check('a deleted board has no link', Notifications.linkFor({ kind: 'board_deleted', board_id: 'b1', task_id: null }), null);
check('a board deletion names the board', Notifications.describe({ kind: 'board_deleted', board_name: 'Work' }), 'deleted the board Work');

replies['/api/notifications/1/read'] = { unread: 2 };
replies['/api/notifications/2/read'] = { unread: 1 };
function fakeList() {
  return {
    children: [],
    appendChild: function (row) { this.children.push(row); },
    textContent: ''
  };
}
var row = function (n) { return n.id; };
Notifications.row = row;

Notifications.markRead([1, 2]).then(function () {
  check('marking rows read leaves the lowest unread count', Notifications.unread, 1);
  var slow;
  fetch = function () { return new Promise(function (resolve) { slow = resolve; }); };
  var list = fakeList();
  var empty = { hidden: true };
  var more = { hidden: true };
  var stale = Notifications.load(list, empty, more, null);
  var staleResolve = slow;
  Notifications.generation++;
  var fresh = Notifications.load(list, empty, more, null);
  slow({ items: [{ id: 9, read: true }], next_before: null, unread: 0 });
  return fresh.then(function () {
    staleResolve({ items: [{ id: 8, read: true }], next_before: null, unread: 0 });
    return stale;
  }).then(function () {
    check('a stale load adds no rows', list.children, [9]);
  });
}).then(function () {
  if (failures) process.exit(1);
  console.log('notifications ok');
});
