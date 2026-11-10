const src = require('fs').readFileSync(process.argv[2], 'utf8');
const slice = (from, to) => {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) throw new Error('not found: ' + from);
  return src.slice(start, end);
};
const helpers = slice('  const WIP_LIMIT_CODE', '  const refuseMove');
const where = slice('    const fillSelect = ', '    // mode: ');

let failures = 0;
const check = (name, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got)));
};

const cssEscape = (s) => String(s);
const putCardBack = () => {};
let watcherCallback = null;
class MutationObserver {
  constructor(callback) { watcherCallback = callback; }
  observe() {}
  disconnect() { watcherCallback = null; }
}
const element = () => {
  const el = { textContent: '', hidden: true, listeners: {}, dataset: {}, parentNode: null };
  el.addEventListener = (type, fn) => { el.listeners[type] = fn; };
  el.focus = () => {};
  el.cloneNode = () => Object.assign(element(), { parentNode: el.parentNode });
  return el;
};
const dialog = element();
let force = element();
force.parentNode = { replaceChild: (fresh) => { force = fresh; } };
const title = element();
const message = element();
const header = { dataset: { columnName: 'Doing' } };
const document = {
  getElementById: (id) => ({ 'wip-dialog': dialog, 'wip-force': force, 'wip-title': title, 'wip-message': message })[id],
  querySelector: () => header,
  createElement: () => element(),
};
const window = {};
const calls = [];
const api = (path, method, body) => {
  calls.push({ path, method, body });
  return Promise.resolve(responses.shift());
};
let responses = [];
const reply = (status, body) => ({ status, ok: status < 400, clone: () => ({ json: () => Promise.resolve(body) }) });
const full = () => reply(409, { code: 'wip_limit_reached', message: 'column WIP limit reached (3/3)' });
eval(helpers + '\nglobalThis.under = { withWip, moveRequest, moveWithWip, withForce, WIP_DECLINED };');
const { withWip, moveRequest, moveWithWip, withForce } = globalThis.under;

const tick = () => new Promise((resolve) => setImmediate(resolve));

(async () => {
  moveRequest('t1', {}, false);
  check('a move does not force by default', calls[0].path, '/api/tasks/t1/move');
  moveRequest('t1', {}, true);
  check('a retry adds force to the query', calls[1].path, '/api/tasks/t1/move?force=true');
  check('a body gains force only on retry', [withForce({ a: 1 }, false), withForce({ a: 1 }, true)],
    [{ a: 1 }, { a: 1, force: true }]);

  const fine = reply(200, null);
  responses = [fine];
  check('an answer that is not 409 passes through', await withWip(() => api('/x'), 'Move anyway') === fine, true);

  const other = reply(409, { code: 'stale_fence', message: 'x' });
  responses = [other];
  check('another 409 passes through', await withWip(() => api('/x'), 'Move anyway') === other, true);

  calls.length = 0;
  responses = [full(), fine];
  const forced = withWip((go) => api('/x', 'POST', { go }), 'Move anyway', 'c1');
  await tick();
  check('a full column opens the dialog', dialog.hidden, false);
  check('the dialog names the column', title.textContent, '"Doing" is full');
  check('the dialog shows the count and limit', message.textContent, 'column WIP limit reached (3/3)');
  check('the force button carries the verb', force.textContent, 'Move anyway');
  force.listeners.click();
  check('Move anyway retries with force', await forced === fine && calls.length === 2 && calls[1].body.go, true);
  check('the dialog closes', dialog.hidden, true);

  calls.length = 0;
  responses = [full()];
  const declined = withWip((go) => api('/x', 'POST', { go }), 'Add anyway');
  await tick();
  dialog.hidden = true;
  watcherCallback();
  check('declining resolves null', await declined, null);
  check('declining sends nothing more', calls.length, 1);

  calls.length = 0;
  responses = [full()];
  const moved = moveWithWip('t9', { column_id: 'c1', position: 0 });
  await tick();
  dialog.hidden = true;
  watcherCallback();
  check('a declined drag leaves the move unforced', await moved === null && calls.length === 1 && calls[0].path.indexOf('force') < 0, true);

  const form = {
    elements: {
      column_id: { value: 'c1' },
      swimlane_id: { value: 'l1' },
      name: { value: 'Draft' },
      description: { value: 'Notes' },
    },
  };
  const spans = (items) => items.map(([id, name]) => ({ dataset: { columnId: id, laneId: id, columnName: name, laneName: name } }));
  const picker = (el) => { el.hidden = true; el.focus = () => { picker.focused = true; }; return el; };
  const columnSelect = picker(element());
  const laneSelect = picker(element());
  columnSelect.appendChild = (option) => { (columnSelect.options = columnSelect.options || []).push(option); };
  laneSelect.appendChild = (option) => { (laneSelect.options = laneSelect.options || []).push(option); };
  const row = element();
  row.querySelector = (sel) => (sel === '[data-add-where-column]' ? columnSelect : laneSelect);
  const original = document.getElementById;
  document.getElementById = (id) => (id === 'add-where' ? row : original(id));
  document.querySelectorAll = (sel) => (sel.indexOf('board-column-options') >= 0
    ? spans([['c1', 'Doing'], ['c2', 'Done']])
    : spans([['l1', 'Top'], ['l2', 'Bottom']]));
  eval(where + '\nglobalThis.under.showWhere = showWhere;');
  globalThis.under.showWhere();
  check('choosing another column shows the picker', row.hidden, false);
  check('the picker lists the columns', columnSelect.options.map((o) => o.textContent), ['Doing', 'Done']);
  check('the picker starts on the current column', columnSelect.options.map((o) => o.selected), [true, false]);
  check('showing the picker keeps every typed field', [form.elements.name.value, form.elements.description.value,
    form.elements.column_id.value, form.elements.swimlane_id.value], ['Draft', 'Notes', 'c1', 'l1']);
  row.listeners.change({ target: { value: 'c2', closest: (sel) => sel === '[data-add-where-column]' } });
  row.listeners.change({ target: { value: 'l2', closest: (sel) => sel === '[data-add-where-lane]' } });
  check('picking another column and swimlane moves the form there and keeps the draft',
    [form.elements.column_id.value, form.elements.swimlane_id.value, form.elements.name.value, form.elements.description.value],
    ['c2', 'l2', 'Draft', 'Notes']);
  process.exit(failures ? 1 : 0);
})();
