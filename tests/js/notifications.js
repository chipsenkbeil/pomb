var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var Notifications = {');
var end = src.indexOf('  function initNotificationsPanel() {', start);
if (start < 0 || end < start) throw new Error('notifications object not found');
const LocalTime = require('../../static/pure.js').LocalTime;
const accountPrefs = { hour12: true };
LocalTime.use(accountPrefs);
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

const fakeEl = (tag) => ({
  tag, children: [], attrs: {}, className: '', textContent: '', title: '',
  setAttribute(k, v) { this.attrs[k] = v; },
  appendChild(c) { this.children.push(c); return c; }
});
const document = { createElement: fakeEl, createTextNode: (text) => ({ text }) };
const timeOf = (created) => {
  const built = Notifications.row({ id: 5, read: true, kind: 'board_deleted', actor: 'a', board_name: 'W', created_at: created });
  return built.children.find((c) => c.tag === 'time');
};
const stamp = new Date(Date.now() - 5 * 60000).toISOString();
check('a row shows relative time', timeOf(stamp).textContent, '5m ago');
check('the hover title is the exact local time', timeOf(stamp).title, LocalTime.write('datetime', Date.parse(stamp), Date.now(), undefined, true));
check('the time element keeps the instant', timeOf(stamp).attrs.datetime, stamp);
const base = Date.parse('2026-11-03T12:00:00Z');
check('seconds read just now', Notifications.ago(Date.parse('2026-11-03T11:59:30Z'), base), 'just now');
check('hours', Notifications.ago(Date.parse('2026-11-03T09:00:00Z'), base), '3h ago');
check('a day is yesterday', Notifications.ago(Date.parse('2026-11-02T11:00:00Z'), base), 'yesterday');
check('days', Notifications.ago(Date.parse('2026-10-30T12:00:00Z'), base), '4d ago');
check('a bad timestamp adds no time', timeOf('nope'), undefined);
const dismissOf = Notifications.row({ id: 5, read: true, kind: 'board_deleted', actor: 'a', board_name: 'W' }).children
  .find((c) => c.className === 'notification-dismiss');
check('a row has a dismiss button', dismissOf && dismissOf.textContent, '×');

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
}).then(() => {
  const requests = [];
  fetch = (url, opts) => { requests.push([url, opts.method]); return Promise.resolve(replies[url]); };
  const kept = { remove() { this.gone = true; } };
  const list = { children: [kept, {}] };
  const empty = { hidden: true };
  const more = { hidden: false };
  replies['/api/notifications/7'] = { unread: 4 };
  return Notifications.dismiss('7', kept, list, empty).then(() => {
    check('dismiss deletes the row on the server', requests[0], ['/api/notifications/7', 'DELETE']);
    check('dismiss removes the row', kept.gone, true);
    check('dismiss shows the new unread count', Notifications.unread, 4);
    check('rows remaining keep the empty message hidden', empty.hidden, true);
    list.children = [];
    return Notifications.dismiss('7', kept, list, empty);
  }).then(() => {
    check('dismissing the last row shows the empty message', empty.hidden, false);
    replies['/api/notifications/7'] = undefined;
    kept.gone = false;
    return Notifications.dismiss('7', kept, list, empty);
  }).then(() => {
    check('a failed dismiss keeps the row', kept.gone, false);
    replies['/api/notifications'] = { unread: 0 };
    list.textContent = 'rows';
    empty.hidden = true;
    Notifications.nextBefore = 3;
    return Notifications.clearAll(list, empty, more);
  }).then(() => {
    check('clear all deletes every notification', requests[requests.length - 1], ['/api/notifications', 'DELETE']);
    check('clear all empties the list', list.textContent, '');
    check('clear all shows the empty message', empty.hidden, false);
    check('clear all hides show older', more.hidden, true);
    check('clear all forgets the cursor', Notifications.nextBefore, null);
    check('clear all zeroes the unread count', Notifications.unread, 0);
  });
}).then(function () {
  if (failures) process.exit(1);
  console.log('notifications ok');
});
