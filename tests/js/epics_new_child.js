var src = require('fs').readFileSync(process.argv[2], 'utf8');
function fn(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}

function element(attrs) {
  var el = {
    hidden: false,
    value: '',
    focused: 0,
    dataset: attrs || {},
    listeners: {},
    focus: function () { el.focused++; },
    addEventListener: function (type, fn) { el.listeners[type] = fn; },
    click: function () { el.listeners.click({}); },
    key: function (key) {
      var ev = {
        key: key,
        prevented: false,
        stopped: false,
        preventDefault: function () { ev.prevented = true; },
        stopPropagation: function () { ev.stopped = true; },
      };
      el.listeners.keydown(ev);
      return ev;
    },
  };
  return el;
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

var modal, row, open, input, create, cancel, calls, toasts, refreshes, pending, modalTab;
var LinkKind = { CHILD: 'child', PARENT: 'parent' };
var modalDirty = false;

function reset(dataset) {
  modal = { dataset: dataset || { taskId: 'e1', columnId: 'c1', swimlaneId: 's1' } };
  row = element();
  row.hidden = true;
  open = element();
  input = element();
  create = element();
  cancel = element();
  calls = [];
  toasts = [];
  refreshes = 0;
  pending = [];
  modalTab = 'description';
  var parts = {
    '[data-new-child-input]': input,
    '[data-new-child-create]': create,
    '[data-new-child-cancel]': cancel,
  };
  row.querySelector = function (sel) { return parts[sel]; };
  var overlay = {
    querySelector: function (sel) {
      if (sel === '[data-new-child-row]') return row;
      if (sel === '[data-new-child-open]') return open;
      return null;
    },
  };
  newChildBusy = false;
  wireNewChild(overlay);
}

var document = {
  querySelector: function (sel) { return sel === '#modal-root .task-modal' ? modal : null; },
};
function api(path, method, data) {
  return new Promise(function (resolve) {
    calls.push({ path: path, method: method, data: data });
    pending.push(resolve);
  });
}
const WIP_DECLINED = new Error('declined');
const withWip = (send) => send(false);
const withForce = (body) => body;
function fetch(path, init) {
  return new Promise(function (resolve) {
    calls.push({ path: path, method: init.method, data: JSON.parse(init.body), headers: init.headers });
    var json = init.headers && init.headers.Accept === 'application/json';
    pending.push(function (res) {
      resolve(json || !res.ok ? res : response(true, '<li class="task-card" data-task-id="n1"></li>'));
    });
  });
}
function toast(message) { toasts.push(message); }
function refreshAfterLinkChange() { refreshes++; }
var newChildBusy = false;
eval(fn('  function linkError(message) {'));
eval(fn('  function linkFailure(err, fallback) {'));
eval(fn('  function taskLinksUrl(taskId) {'));
eval(fn('  function addTaskLink(taskId, kind, targetId) {'));
eval(fn('  function closeNewChild(row, open) {'));
eval(fn('  function createNewChild(row) {'));
eval(fn('  function wireNewChild(overlay) {'));

(async function () {
  reset();
  open.click();
  check('the button shows the row', row.hidden, false);
  check('the button focuses the name field', input.focused, 1);

  input.value = 'Retry rules';
  var enter = input.key('Enter');
  check('enter is stopped', [enter.prevented, enter.stopped], [true, true]);
  check('enter creates the card in the epic column and lane', calls, [
    {
      path: '/api/tasks',
      method: 'POST',
      data: { column_id: 'c1', name: 'Retry rules', swimlane_id: 's1' },
      headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
    },
  ]);
  input.key('Enter');
  create.click();
  check('a second enter or click while busy posts nothing', calls.length, 1);
  pending[0](response(true, '{"id":"n1"}'));
  await flush();
  await flush();
  check('the child link follows the create, epic to child', calls[1], {
    path: '/api/tasks/e1/links',
    method: 'POST',
    data: { kind: 'child', target_id: 'n1' },
  });
  check('no refresh before the link lands', refreshes, 0);
  pending[1](response(true, ''));
  await flush();
  await flush();
  check('success refreshes the epic cards once', refreshes, 1);
  check('success toasts nothing', toasts, []);
  check('success lands on the epic cards', modalTab, 'children');

  reset({ taskId: 'e1', columnId: 'c1' });
  open.click();
  input.value = 'No lane';
  create.click();
  check('a column without a lane posts no swimlane_id', calls[0].data, { column_id: 'c1', name: 'No lane' });

  reset();
  open.click();
  input.value = '   ';
  input.key('Enter');
  create.click();
  check('a blank name posts nothing', calls.length, 0);
  check('a blank name keeps the row open and refocuses', [row.hidden, input.focused > 1], [false, true]);

  reset();
  open.click();
  input.value = 'Draft';
  var esc = input.key('Escape');
  check('escape is stopped', [esc.prevented, esc.stopped], [true, true]);
  check('escape hides the row and clears the draft', [row.hidden, input.value, calls.length], [true, '', 0]);
  check('escape returns focus to the new child button', open.focused, 1);
  open.click();
  input.value = 'Draft';
  cancel.click();
  check('cancel hides the row and clears the draft', [row.hidden, input.value, calls.length], [true, '', 0]);
  check('cancel returns focus to the new child button', open.focused, 2);

  reset();
  open.click();
  input.value = 'Retry rules';
  var letter = input.key('a');
  check('a letter key does not reach the document', [letter.stopped, letter.prevented], [true, false]);

  reset();
  open.click();
  input.value = 'Retry rules';
  create.click();
  pending[0](response(false, '{"message":"column is full"}'));
  await flush();
  await flush();
  check('a failed create toasts the server message', toasts, ['column is full']);
  check('a failed create links nothing and refreshes nothing', [calls.length, refreshes], [1, 0]);
  check('a failed create keeps the row and the name', [row.hidden, input.value], [false, 'Retry rules']);
  create.click();
  check('a failed create can be retried', calls.length, 2);

  reset();
  open.click();
  input.value = 'Retry rules';
  create.click();
  pending[0](response(false, ''));
  await flush();
  await flush();
  check('an empty refusal falls back to a fixed message', toasts, ['Could not create the card.']);

  reset();
  open.click();
  input.value = 'Retry rules';
  create.click();
  pending[0](response(true, '{"id":"n1"}'));
  await flush();
  await flush();
  pending[1](response(false, '{"message":"not allowed"}'));
  await flush();
  await flush();
  check('a failed link says the card exists but is not in the epic', toasts, [
    'Created "Retry rules" but could not add it to this epic. It is in the same column.',
  ]);
  check('a failed link still refreshes the epic cards', refreshes, 1);
  check('a failed link does not retry the create', calls.length, 2);
  create.click();
  check('the busy flag is cleared after a failed link', calls.length, 3);

  console.log('failures=' + failures);
  if (failures) throw new Error(failures + ' new child check(s) failed');
})();
