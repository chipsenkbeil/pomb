var src = require('fs').readFileSync(process.argv[2], 'utf8');
function slice(from, to) {
  var start = src.indexOf(from);
  var end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
}

var members = [
  { id: '1', username: 'ada', display_name: 'Ada Lovelace' },
  { id: '2', username: 'bob', display_name: 'Bobby' },
  { id: '3', username: 'abe', display_name: 'abe' }
];
var calls = [];
function api(path, method, data) {
  calls.push([method, path, data]);
  return Promise.resolve({ ok: true, json: function () { return Promise.resolve(members); } });
}
function makeEl(tag) {
  return {
    tag: tag, children: [], attrs: {}, listeners: {}, style: {}, hidden: false, textContent: '',
    setAttribute: function (k, v) { this.attrs[k] = v; },
    appendChild: function (c) { this.children.push(c); },
    addEventListener: function (t, fn) { (this.listeners[t] = this.listeners[t] || []).push(fn); },
    getBoundingClientRect: function () { return { left: 1, bottom: 2 }; },
    focus: function () {}, setSelectionRange: function (a) { this.selectionStart = a; }
  };
}
Object.defineProperty(makeEl('x'), 'x', { value: 1 });
var body = makeEl('body');
var input = makeEl('input');
var document = {
  createElement: makeEl,
  body: body,
  activeElement: input
};
var SETTLE_MS = 20;
eval(slice('  var MENTION_LIST_MAX = 8;', '  function wireModalTaskRefs'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

check('an @ at the start is a mention', mentionAt('@a', 2), { start: 0, query: 'a' });
check('an @ after a space is a mention', mentionAt('hi @Ad', 6), { start: 3, query: 'ad' });
check('an email is not a mention', mentionAt('me@a', 4), null);
check('a space after the name ends the mention', mentionAt('@ada ', 5), null);
check('a bare @ lists everyone', mentionAt('x @', 3), { start: 2, query: '' });

check('the query narrows by username', mentionMatches(members, 'bo').map(function (m) { return m.username; }), ['bob']);
check('the query narrows by display name', mentionMatches(members, 'love').map(function (m) { return m.username; }), ['ada']);
check('an empty query lists all', mentionMatches(members, '').length, 3);

check('choosing inserts the username and a space', mentionInsert('hi @a there', 5, { start: 3, query: 'a' }, 'ada'), { value: 'hi @ada there', caret: 7 });

input.value = '';
input.selectionStart = 0;
wireMentions(input);
function fire(type, ev) { (input.listeners[type] || []).forEach(function (fn) { fn(ev); }); }

input.value = 'hello @a';
input.selectionStart = 8;
fire('input', {});
Promise.resolve().then(function () { return Promise.resolve(); }).then(function () { return Promise.resolve(); }).then(function () {
  var pop = body.children[0];
  check('typing @a opens the list', pop && !pop.hidden && pop.children.map(function (b) { return b.attrs['data-mention']; }), ['ada', 'abe']);
  check('members are fetched from the members route', calls[0].slice(0, 2), ['GET', '/api/members']);
  var enterEvent = { key: 'Enter', preventDefault: function () {}, stopImmediatePropagation: function () { enterEvent.stopped = true; } };
  fire('keydown', { key: 'ArrowDown', preventDefault: function () {} });
  fire('keydown', enterEvent);
  check('Enter chooses the highlighted member', input.value, 'hello @abe ');
  check('Enter does not reach the submit handler', enterEvent.stopped, true);
  check('the list closes after choosing', pop.hidden, true);
  input.value = 'plain';
  input.selectionStart = 5;
  fire('input', {});
  check('text without @ opens nothing', pop.hidden, true);

  var task = { watched: false };
  var posts = [];
  function api2(path, method, data) {
    posts.push([method, path]);
    return Promise.resolve({ ok: true, json: function () { return Promise.resolve({}); } });
  }
  var ta = { value: ' note ', focus: function () {} };
  var documentWatch = { getElementById: function (id) { return id === 'modal-comment-input' ? ta : null; } };
  var watchUI = [];
  var refreshed = 0;
  function modalTaskId() { return 't1'; }
  function modalWatchedState() { return task.watched; }
  function setModalWatchUI(v) { watchUI.push(v); }
  function refreshModal() { refreshed++; }
  function toast() {}
  var addSrc = slice('  function addModalComment() {', '  function closeCommentEditors');
  (function (api, document) { eval(addSrc); addModalComment(); })(api2, documentWatch);
  return new Promise(function (r) { setTimeout(r, SETTLE_MS); }).then(function () {
    check('a comment posts then watches', posts, [['POST', '/api/tasks/t1/comments'], ['POST', '/api/tasks/t1/watch']]);
    check('the watch button follows', watchUI, [true]);
    check('the modal refreshes', refreshed, 1);
    task.watched = true;
    posts.length = 0;
    (function (api, document) { eval(addSrc); addModalComment(); })(api2, documentWatch);
    return new Promise(function (r) { setTimeout(r, SETTLE_MS); });
  }).then(function () {
    check('an already watched task is not watched again', posts, [['POST', '/api/tasks/t1/comments']]);
  });
}).then(function () {
  if (failures) process.exit(1);
  console.log('ok');
});
