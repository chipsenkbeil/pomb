const src = require('fs').readFileSync(process.argv[2], 'utf8');
function fn(start) {
  const from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}

let failures = 0;
function check(name, got, want) {
  const g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

const hiddenDd = { hidden: true, dataset: { row: 'due' }, previousElementSibling: { textContent: 'Due' } };
const shownDd = { hidden: false, dataset: { row: 'labels' }, previousElementSibling: { textContent: 'Labels' } };
const metadata = { hidden: true };
const menu = { open: true, hidden: true, items: [], querySelector: () => items };
const items = { textContent: '', children: [], appendChild(node) { this.children.push(node); } };
const clicks = [];
const edit = { click: () => { clicks.push('edit'); } };
const opened = [];
let focused = null;
const control = { focus: () => { focused = 'control'; } };
const cells = [hiddenDd, { hidden: true }];

const document = {
  getElementById(id) {
    if (id === 'modal-metadata-wrap') return metadata;
    if (id === 'tm-add-field-menu') return menu;
    return null;
  },
  querySelector(selector) {
    if (selector.indexOf('data-metadata-edit') !== -1) return edit;
    return control;
  },
  querySelectorAll(selector) {
    if (selector.indexOf('[hidden]') !== -1) return [hiddenDd].filter((dd) => dd.hidden);
    return cells;
  },
};
function sideEl(tag, className, text) { return { tag, className, textContent: text, dataset: {} }; }
function fieldGroupModel() { return [{ name: 'a', editable: true, shown: [] }, { name: 'b', editable: true, shown: [1] }]; }
function openFieldEditor(name) { opened.push(name); }
const ADD_FIELD = Object.freeze({ ROW: 'addRow', GROUP: 'addGroup', METADATA: 'addMetadata' });
eval(fn('  function addFieldEntries(groups) {'));
eval(fn('  function renderAddFieldMenu(groups) {'));
eval(fn('  function pickAddField(item) {'));

renderAddFieldMenu(fieldGroupModel());
check('menu_should_list_hidden_rows_empty_groups_and_metadata',
  items.children.map((n) => [n.textContent, Object.keys(n.dataset)[0]]),
  [['Due', 'addRow'], ['Fields · a', 'addGroup'], ['Metadata', 'addMetadata']]);
check('menu_should_show_when_it_has_entries', menu.hidden, false);

pickAddField(items.children[0]);
check('menu_should_reveal_the_picked_row', [cells[0].hidden, cells[1].hidden, focused, menu.open], [false, false, 'control', false]);

pickAddField(items.children[1]);
pickAddField(items.children[2]);
check('menu_should_open_the_group_editor_and_the_metadata_dialog', [opened, clicks], [['a'], ['edit']]);

console.log('failures=' + failures);
if (failures) throw new Error(failures + ' add field menu check(s) failed');
