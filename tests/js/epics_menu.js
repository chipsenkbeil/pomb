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

function response(ok, body) {
  return { ok: ok, text: function () { return Promise.resolve(body); } };
}

var modal, nameInput, calls, toasts, confirms, refreshes, closes, replies, modalDirty, timerRefreshes;
var window = {};

function reset(dataset, name) {
  modal = dataset ? { dataset: dataset } : null;
  nameInput = { value: name === undefined ? 'Column limits' : name };
  calls = [];
  toasts = [];
  confirms = [];
  refreshes = 0;
  closes = 0;
  replies = [];
  modalDirty = false;
  timerRefreshes = 0;
  window = { TimerUI: { refresh: function () { timerRefreshes++; }, broadcastTimer: function () {} } };
}

var document = {
  querySelector: function (sel) { return sel === '#modal-root .task-modal.tm-epic' ? modal : null; },
  getElementById: function (id) { return id === 'modal-name' ? nameInput : null; }
};
function modalTaskId() { return modal ? 'e1' : null; }
function fetch(path, init) {
  calls.push({ path: path, method: init.method, headers: init.headers, body: init.body });
  return Promise.resolve(replies.shift());
}
function toast(message) { toasts.push(message); }
function showConfirmDialog(title, message, okLabel, onConfirm) {
  confirms.push({ title: title, message: message, okLabel: okLabel, ok: onConfirm });
}
function refreshAfterLinkChange() { refreshes++; }
function closeModal() { closes++; }
eval(fn('  function linkError(message) {'));
eval(fn('  function linkFailure(err, fallback) {'));
eval(fn('  function epicModalFacts() {'));
eval(fn('  function epicRequest(path, method, body) {'));
eval(fn('  function setEpicClosed(id, closed) {'));
eval(fn('  function closeModalEpic() {'));
eval(fn('  function epicCardsPhrase(total) {'));
eval(fn('  function unmakeModalEpic() {'));
eval(fn('  function deleteModalEpic(id, epic) {'));
eval(fn('  function deleteModalTask() {'));

var JSON_HEADERS = { 'Content-Type': 'application/json', 'Accept': 'application/json' };

(async function () {
  reset({ childrenDone: '3', childrenTotal: '5' });
  closeModalEpic();
  check('closing with open children asks first', confirms.map(function (c) {
    return [c.title, c.message, c.okLabel];
  }), [['Close epic', '2 of 5 cards are still open. Close anyway? They stay where they are.', 'Close anyway']]);
  check('asking sends nothing', calls, []);
  await flush();
  check('a cancelled confirm sends nothing and refreshes nothing', [calls.length, refreshes, toasts], [0, 0, []]);
  replies.push(response(true, '{}'));
  confirms[0].ok();
  await flush();
  await flush();
  check('confirming posts the close route with the JSON Accept header', calls, [
    { path: '/api/tasks/e1/close', method: 'POST', headers: JSON_HEADERS, body: undefined }
  ]);
  check('success refreshes the modal once', refreshes, 1);

  reset({ childrenDone: '5', childrenTotal: '5' });
  replies.push(response(true, '{}'));
  closeModalEpic();
  await flush();
  await flush();
  check('closing with no open child does not ask', confirms.length, 0);
  check('closing with no open child posts the close route', calls.map(function (c) { return c.path; }), ['/api/tasks/e1/close']);

  reset({ childrenDone: '0', childrenTotal: '0' });
  replies.push(response(true, '{}'));
  closeModalEpic();
  check('an empty epic closes without a question', [confirms.length, calls.length], [0, 1]);
  await flush();
  await flush();

  reset({ childrenDone: '1', childrenTotal: '2' });
  closeModalEpic();
  replies.push(response(false, '{"message":"closing needs epics.manage"}'));
  confirms[0].ok();
  await flush();
  await flush();
  check('a refused close toasts the server message', toasts, ['closing needs epics.manage']);
  check('a refused close refreshes nothing', refreshes, 0);

  reset({ childrenDone: '1', childrenTotal: '1' });
  replies.push(response(false, ''));
  closeModalEpic();
  await flush();
  await flush();
  check('an empty refusal falls back to a fixed close message', toasts, ['Could not close the epic.']);

  reset({ childrenDone: '1', childrenTotal: '1' });
  fetch = function () { return Promise.reject(new Error('offline')); };
  closeModalEpic();
  await flush();
  await flush();
  check('a network failure falls back to a fixed close message', [toasts, refreshes], [['Could not close the epic.'], 0]);
  fetch = function (path, init) {
    calls.push({ path: path, method: init.method, headers: init.headers, body: init.body });
    return Promise.resolve(replies.shift());
  };

  reset({ childrenDone: '0', childrenTotal: '1' });
  replies.push(response(true, '{}'));
  setEpicClosed('e1', false);
  await flush();
  await flush();
  check('reopen posts the reopen route with the JSON Accept header', calls, [
    { path: '/api/tasks/e1/reopen', method: 'POST', headers: JSON_HEADERS, body: undefined }
  ]);
  check('reopen refreshes the modal once', refreshes, 1);
  replies.push(response(false, '{"message":"the epic is not closed"}'));
  setEpicClosed('e1', false);
  await flush();
  await flush();
  check('a refused reopen toasts the server message', [toasts, refreshes], [['the epic is not closed'], 1]);
  replies.push(response(false, ''));
  setEpicClosed('e1', false);
  await flush();
  await flush();
  check('an empty refusal falls back to a fixed reopen message', toasts[1], 'Could not reopen the epic.');

  reset({ childrenDone: '1', childrenTotal: '5' }, 'Column limits');
  deleteModalTask();
  check('deleting an epic confirms with what happens to its cards', confirms.map(function (c) {
    return [c.title, c.message, c.okLabel];
  }), [['Delete epic',
    'Delete "Column limits"? It goes to the recycle bin. Its 5 cards stay on the board and lose their link to it. Restoring the epic from the bin does not link them again.',
    'Delete epic']]);
  check('asking sends nothing', calls, []);
  replies.push(response(true, ''));
  confirms[0].ok();
  await flush();
  await flush();
  check('confirming deletes the task with the JSON Accept header', calls, [
    { path: '/api/tasks/e1', method: 'DELETE', headers: JSON_HEADERS, body: undefined }
  ]);
  check('success closes the modal, marks it dirty and refreshes the timer', [closes, modalDirty, timerRefreshes], [1, true, 1]);

  reset({ childrenDone: '0', childrenTotal: '1' }, 'Solo');
  deleteModalTask();
  check('one card reads in the singular', confirms[0].message,
    'Delete "Solo"? It goes to the recycle bin. Its 1 card stays on the board and loses its link to it. Restoring the epic from the bin does not link them again.');
  reset({ childrenDone: '0', childrenTotal: '0' }, 'Empty');
  deleteModalTask();
  check('an empty epic mentions no cards', confirms[0].message, 'Delete "Empty"? It goes to the recycle bin.');
  replies.push(response(false, '{"message":"card is locked"}'));
  confirms[0].ok();
  await flush();
  await flush();
  check('a refused delete toasts the server message and keeps the modal', [toasts, closes, modalDirty], [['card is locked'], 0, false]);
  replies.push(response(false, ''));
  deleteModalTask();
  confirms[1].ok();
  await flush();
  await flush();
  check('an empty refusal falls back to a fixed delete message', toasts[1], 'Could not delete the epic.');

  reset({ childrenDone: '1', childrenTotal: '3' }, 'Column limits');
  unmakeModalEpic();
  check('make it a card confirms first', confirms.map(function (c) {
    return [c.title, c.message, c.okLabel];
  }), [['Make it a card',
    'Make "Column limits" an ordinary card? Its 3 cards stay on the board and lose their link to it.',
    'Make it a card']]);
  await flush();
  check('a cancelled confirm sends nothing', calls, []);
  replies.push(response(true, '{}'));
  confirms[0].ok();
  await flush();
  await flush();
  check('confirming patches kind task with unlink_children and the JSON Accept header', calls, [
    { path: '/api/tasks/e1', method: 'PATCH', headers: JSON_HEADERS, body: '{"kind":"task","unlink_children":true}' }
  ]);
  check('success refreshes the modal once', refreshes, 1);

  reset({ childrenDone: '0', childrenTotal: '0' }, 'Empty');
  unmakeModalEpic();
  check('an empty epic mentions no cards in the make-a-card confirm', confirms[0].message, 'Make "Empty" an ordinary card?');
  replies.push(response(false, '{"message":"a child is locked"}'));
  confirms[0].ok();
  await flush();
  await flush();
  check('a refused make-a-card toasts the server message', [toasts, refreshes], [['a child is locked'], 0]);
  replies.push(response(false, ''));
  unmakeModalEpic();
  confirms[1].ok();
  await flush();
  await flush();
  check('an empty refusal falls back to a fixed make-a-card message', toasts[1], 'Could not make the epic a card.');

  reset(null);
  closeModalEpic();
  unmakeModalEpic();
  deleteModalTask();
  check('with no epic modal open nothing happens', [confirms.length, calls.length], [0, 0]);

  console.log('failures=' + failures);
  if (failures) throw new Error(failures + ' epic menu check(s) failed');
})();
