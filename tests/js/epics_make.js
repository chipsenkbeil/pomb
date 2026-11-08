var src = require('fs').readFileSync(process.argv[2], 'utf8');
function fn(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

function flush() {
  return new Promise(function (resolve) { setImmediate(resolve); });
}

function settle() {
  return flush().then(flush).then(flush).then(flush).then(flush).then(flush).then(flush).then(flush);
}

function response(ok, body) {
  return { ok: ok, json: function () { return Promise.resolve(JSON.parse(body)); }, text: function () { return Promise.resolve(body); } };
}

function node(tag) {
  var el = { tag: tag, children: [], hidden: false, textContent: '', checked: false, focused: 0, listeners: {} };
  el.appendChild = function (child) { el.children.push(child); return child; };
  el.addEventListener = function (type, handler) { el.listeners[type] = handler; };
  el.focus = function () { el.focused++; };
  el.cloneNode = function () { var copy = node(tag); copy.textContent = el.textContent; return copy; };
  el.parentNode = { replaceChild: function (fresh, old) { els[old.id] = fresh; fresh.id = old.id; } };
  Object.defineProperty(el, 'textContent', {
    get: function () { return el.text || ''; },
    set: function (value) { el.text = value; if (value === '') el.children = []; },
  });
  return el;
}

var els, calls, toasts, replies, finishes, reloads, stored;
var window = {};

function reset() {
  els = {};
  ['make-epic-dialog', 'make-epic-message', 'make-epic-list', 'make-epic-subtasks', 'make-epic-ok', 'modal-name'].forEach(function (id) {
    els[id] = node('x');
    els[id].id = id;
  });
  els['make-epic-dialog'].hidden = true;
  els['modal-name'].value = 'Column limits';
  calls = [];
  toasts = [];
  replies = [];
  finishes = [];
  reloads = 0;
  stored = {};
  window = {
    sessionStorage: { setItem: function (k, v) { stored[k] = v; } },
    location: { reload: function () { reloads++; } },
  };
}

var document = {
  getElementById: function (id) { return els[id] || null; },
  createElement: function (tag) { return node(tag); },
};
function fetch(path, init) {
  calls.push({ path: path, method: (init && init.method) || 'GET', headers: init && init.headers, body: init && init.body });
  var reply = replies.shift();
  return reply instanceof Error ? Promise.reject(reply) : Promise.resolve(reply);
}
function toast(message) { toasts.push(message); }
function refreshAfterLinkChange() { finishes.push('refresh'); }
function fetchJson(url) {
  return fetch(url, { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
    .then(function (res) {
      if (!res.ok) throw new Error('HTTP ' + res.status);
      return res.json();
    });
}
eval(fn('  function linkError(message) {'));
eval(fn('  function linkFailure(err, fallback) {'));
eval(fn('  function epicRequest(path, method, body) {'));
eval(fn('  function showMakeEpicDialog(name, subs, onConfirm) {'));
eval(fn('  function subtaskProblem(sub, linking, err) {'));
eval(fn('  function turnSubtaskIntoChild(epicId, sub) {'));
eval(fn('  function makeEpicProblems(problems) {'));
eval(fn('  function makeEpic(id, picked, finish) {'));
eval(fn('  function startMakeEpic(id, name, finish) {'));
eval(fn('  function makeModalEpic(id) {'));
eval(fn('  function makeCardEpic(task) {'));

var JSON_HEADERS = { 'Content-Type': 'application/json', 'Accept': 'application/json' };
var GET_HEADERS = { 'Accept': 'application/json' };
var SUBS = [{ id: 's1', name: 'Write docs' }, { id: 's2', name: 'Add tests' }, { id: 's3', name: 'Ship it' }];

function dialogBoxes() {
  return els['make-epic-list'].children.map(function (label) { return label.children[0]; });
}

(async function () {
  reset();
  replies.push(response(true, JSON.stringify({ id: 'c1', subtasks: SUBS })));
  makeCardEpic({ id: 'c1', name: 'Column limits' });
  await settle();
  check('opening reads the card with the JSON Accept header and sends nothing else', calls, [
    { path: '/api/tasks/c1', method: 'GET', headers: GET_HEADERS, body: undefined },
  ]);
  check('the dialog opens', els['make-epic-dialog'].hidden, false);
  check('the dialog names the card', els['make-epic-message'].text.indexOf('"Column limits" leaves its column and becomes an epic.') === 0, true);
  check('the subtask list is shown', els['make-epic-subtasks'].hidden, false);
  check('every subtask is listed by name', els['make-epic-list'].children.map(function (l) { return l.children[1].text; }),
    ['Write docs', 'Add tests', 'Ship it']);
  check('all subtasks are unticked', dialogBoxes().map(function (b) { return b.checked; }), [false, false, false]);
  check('the checkboxes are checkboxes', dialogBoxes().map(function (b) { return b.type; }), ['checkbox', 'checkbox', 'checkbox']);

  replies.push(response(true, '{}'));
  els['make-epic-ok'].listeners.click();
  await settle();
  check('confirming with nothing ticked patches the kind only', calls.slice(1), [
    { path: '/api/tasks/c1', method: 'PATCH', headers: JSON_HEADERS, body: '{"kind":"epic"}' },
  ]);
  check('the dialog closes on confirm', els['make-epic-dialog'].hidden, true);
  check('success reloads the board with no toast waiting', [reloads, stored], [1, {}]);

  reset();
  replies.push(response(true, JSON.stringify({ id: 'c1', subtasks: SUBS })));
  makeCardEpic({ id: 'c1', name: 'Column limits' });
  await settle();
  var boxes = dialogBoxes();
  boxes[0].checked = true;
  boxes[2].checked = true;
  replies.push(response(true, '{}'));
  replies.push(response(true, '{"id":"n1"}'));
  replies.push(response(true, '{}'));
  replies.push(response(true, '{"id":"n3"}'));
  replies.push(response(true, '{}'));
  els['make-epic-ok'].listeners.click();
  await settle();
  check('confirming patches, then converts and links each ticked subtask in order', calls.slice(1), [
    { path: '/api/tasks/c1', method: 'PATCH', headers: JSON_HEADERS, body: '{"kind":"epic"}' },
    { path: '/api/tasks/c1/subtasks/s1/convert', method: 'POST', headers: JSON_HEADERS, body: '{"relation":"none"}' },
    { path: '/api/tasks/c1/links', method: 'POST', headers: JSON_HEADERS, body: '{"kind":"child","target_id":"n1"}' },
    { path: '/api/tasks/c1/subtasks/s3/convert', method: 'POST', headers: JSON_HEADERS, body: '{"relation":"none"}' },
    { path: '/api/tasks/c1/links', method: 'POST', headers: JSON_HEADERS, body: '{"kind":"child","target_id":"n3"}' },
  ]);
  check('a clean run reloads once and toasts nothing', [reloads, stored, toasts], [1, {}, []]);

  reset();
  replies.push(response(true, JSON.stringify({ id: 'c1', subtasks: [] })));
  replies.push(response(true, '{}'));
  makeCardEpic({ id: 'c1', name: 'Column limits' });
  await settle();
  check('a card with no subtasks converts straight away without a dialog', [calls.map(function (c) { return c.method + ' ' + c.path; }), els['make-epic-dialog'].hidden, reloads],
    [['GET /api/tasks/c1', 'PATCH /api/tasks/c1'], true, 1]);
  check('the straight conversion sends the epic kind', calls[1].body, '{"kind":"epic"}');
  check('the empty dialog was never filled', els['make-epic-list'].children.length, 0);

  reset();
  replies.push(response(true, JSON.stringify({ id: 'c1', subtasks: SUBS })));
  makeCardEpic({ id: 'c1', name: 'Column limits' });
  await settle();
  dialogBoxes()[1].checked = true;
  els['make-epic-dialog'].hidden = true;
  await settle();
  check('cancelling sends nothing beyond the read and reloads nothing', [calls.length, reloads, toasts], [1, 0, []]);

  reset();
  replies.push(response(true, JSON.stringify({ id: 'c1', subtasks: SUBS })));
  makeCardEpic({ id: 'c1', name: 'Column limits' });
  await settle();
  dialogBoxes().forEach(function (b) { b.checked = true; });
  replies.push(response(false, '{"message":"a card with a parent cannot be an epic"}'));
  els['make-epic-ok'].listeners.click();
  await settle();
  check('a refused patch converts nothing', calls.length, 2);
  check('a refused patch toasts the server message and does not reload', [toasts, reloads], [['a card with a parent cannot be an epic'], 0]);

  reset();
  replies.push(response(true, JSON.stringify({ id: 'c1', subtasks: SUBS })));
  makeCardEpic({ id: 'c1', name: 'Column limits' });
  await settle();
  dialogBoxes().forEach(function (b) { b.checked = true; });
  replies.push(response(true, '{}'));
  replies.push(response(false, '{"message":"the column refuses the card"}'));
  replies.push(response(true, '{"id":"n2"}'));
  replies.push(response(false, ''));
  replies.push(response(true, '{"id":"n3"}'));
  replies.push(response(true, '{}'));
  els['make-epic-ok'].listeners.click();
  await settle();
  check('a failed convert does not stop the remaining subtasks', calls.slice(2).map(function (c) { return c.method + ' ' + c.path; }), [
    'POST /api/tasks/c1/subtasks/s1/convert',
    'POST /api/tasks/c1/subtasks/s2/convert',
    'POST /api/tasks/c1/links',
    'POST /api/tasks/c1/subtasks/s3/convert',
    'POST /api/tasks/c1/links',
  ]);
  check('the failed link of a converted card is not retried', calls.filter(function (c) { return c.path.endsWith('/links'); }).length, 2);
  check('partial failure reloads with a pending toast naming both groups', [reloads, stored['pomb.pendingToast']], [1,
    'These subtasks could not become cards and stay subtasks: "Write docs" (the column refuses the card). ' +
    'These became cards in the same column but are not in the epic: "Add tests".']);

  reset();
  fetch = function () { return Promise.reject(new Error('offline')); };
  makeCardEpic({ id: 'c1', name: 'Column limits' });
  await settle();
  check('a failed read toasts and opens nothing', [toasts, els['make-epic-dialog'].hidden, reloads], [['Could not load the card.'], true, 0]);

  fetch = function (path, init) {
    calls.push({ path: path, method: (init && init.method) || 'GET', headers: init && init.headers, body: init && init.body });
    var reply = replies.shift();
    return reply instanceof Error ? Promise.reject(reply) : Promise.resolve(reply);
  };

  reset();
  replies.push(response(true, JSON.stringify({ id: 'c1', subtasks: SUBS })));
  makeModalEpic('c1');
  await settle();
  boxes = dialogBoxes();
  boxes[1].checked = true;
  replies.push(response(true, '{}'));
  replies.push(response(true, '{"id":"n2"}'));
  replies.push(response(true, '{}'));
  els['make-epic-ok'].listeners.click();
  await settle();
  check('the modal path names the card from the title field', els['make-epic-message'].text.indexOf('"Column limits"') === 0, true);
  check('the modal path sends the same requests', calls.slice(1).map(function (c) { return c.method + ' ' + c.path; }), [
    'PATCH /api/tasks/c1',
    'POST /api/tasks/c1/subtasks/s2/convert',
    'POST /api/tasks/c1/links',
  ]);
  check('the modal path refreshes the modal and does not reload', [finishes, reloads, toasts], [['refresh'], 0, []]);

  reset();
  replies.push(response(true, JSON.stringify({ id: 'c1', subtasks: SUBS })));
  makeModalEpic('c1');
  await settle();
  dialogBoxes()[0].checked = true;
  replies.push(response(true, '{}'));
  replies.push(response(false, '{"message":"card is locked"}'));
  els['make-epic-ok'].listeners.click();
  await settle();
  check('the modal path refreshes and toasts a failure', [finishes, toasts],
    [['refresh'], ['These subtasks could not become cards and stay subtasks: "Write docs" (card is locked).']]);

  console.log('failures=' + failures);
  if (failures) throw new Error(failures + ' make-epic check(s) failed');
})();
