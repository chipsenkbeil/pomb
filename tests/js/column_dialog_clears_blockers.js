const src = require('fs').readFileSync(process.argv[2], 'utf8');
const slice = (from, to) => {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
};

const elements = {};
const element = (id) => (elements[id] = elements[id] || { value: '', checked: false, hidden: true, focus: () => {} });
const header = { dataset: { columnName: 'Release', clearsBlockers: '1' } };
const document = {
  getElementById: element,
  querySelector: () => header,
};
const patches = [];
const toast = () => {};
const cssEscape = (value) => value;
const drained = (res) => res;
const columnConfigBag = () => ({});
const columnPropConfig = () => ({ due_dates: 'show' });
const api = (url, method, body) => {
  patches.push({ url, method, body });
  return Promise.resolve({ ok: false });
};
let editColumnId = null;
const window = { location: { reload: () => {} } };
eval(slice('  function openEditColumnDialog(id) {', '\n  // ---------- swimlanes'));

let failures = 0;
const check = (name, got, want) => {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
};

const run = async () => {
  openEditColumnDialog('c1');
  check('column_dialog_should_save_clears_blockers: the dialog loads the saved flag', element('ec-clears-blockers').checked, true);
  doSaveColumn();
  check('column_dialog_should_save_clears_blockers: a checked toggle saves true', patches[0].body.clears_blockers, true);

  element('ec-clears-blockers').checked = false;
  doSaveColumn();
  check('column_dialog_should_save_clears_blockers: an unchecked toggle saves false', patches[1].body.clears_blockers, false);

  header.dataset.clearsBlockers = '0';
  openEditColumnDialog('c1');
  check('column_dialog_should_save_clears_blockers: an unflagged column loads unchecked', element('ec-clears-blockers').checked, false);

  if (failures) process.exit(1);
  console.log('column_dialog_clears_blockers ok');
};
run();
