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
let seatsGroup = null;
const menu = { open: true, hidden: true, items: [], querySelector: () => items };
const items = { children: [], set textContent(value) { this.children.length = 0; }, appendChild(node) { this.children.push(node); } };
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
    if (id === 'tm-group-seats') return seatsGroup;
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
const ADD_FIELD = Object.freeze({ ROW: 'addRow', GROUP: 'addGroup', METADATA: 'addMetadata', SEATS: 'addSeats' });
eval(fn('  function addFieldEntries(groups) {'));
eval(fn('  function renderAddFieldMenu(groups) {'));
eval(fn('  function pickAddField(item) {'));

renderAddFieldMenu(fieldGroupModel());
check('menu_should_list_hidden_rows_empty_groups_and_metadata',
  items.children.map((n) => [n.textContent, Object.keys(n.dataset)[0]]),
  [['Due', 'addRow'], ['Fields · a', 'addGroup'], ['Metadata', 'addMetadata']]);
check('menu_should_show_when_it_has_entries', menu.hidden, false);

const [rowItem, groupItem, metadataItem] = items.children;
pickAddField(rowItem);
check('menu_should_reveal_the_picked_row', [cells[0].hidden, cells[1].hidden, focused, menu.open], [false, false, 'control', false]);

pickAddField(groupItem);
pickAddField(metadataItem);
check('menu_should_open_the_group_editor_and_the_metadata_dialog', [opened, clicks], [['a'], ['edit']]);

seatsGroup = { hidden: true };
renderAddFieldMenu(fieldGroupModel());
const reviewers = items.children.find((n) => n.textContent === 'Reviewers');
check('menu_should_offer_reviewers_when_the_card_has_no_seats', reviewers ? Object.keys(reviewers.dataset)[0] : null, 'addSeats');
pickAddField(reviewers);
items.children.length = 0;
renderAddFieldMenu(fieldGroupModel());
check('menu_should_unhide_the_reviewers_group_when_picked', [seatsGroup.hidden, items.children.some((n) => n.textContent === 'Reviewers')], [false, false]);

const listsMetadata = () => items.children.some((n) => n.textContent === 'Metadata');
let confirmed = null;
const showConfirmDialog = (title, body, label, action) => { confirmed = action; };
const metadataEditor = { hidden: true };
const metadataEdit = { hidden: false, addEventListener(type, fn) { this.onClick = fn; } };
const sink = { addEventListener() {} };
metadata.querySelector = (s) => ({ '#tm-metadata-editor': metadataEditor, '[data-metadata-edit]': metadataEdit, '#tm-metadata-input': { focus() {} } })[s] || sink;
eval(fn('  function wireMetadata(overlay) {'));
wireMetadata({ querySelector: (s) => (s === '#modal-metadata-wrap' ? metadata : null) });
metadata.hidden = true;
renderAddFieldMenu(fieldGroupModel());
metadataEdit.onClick();
confirmed();
const shownAfterEdit = metadata.hidden;
const listedAfterEdit = listsMetadata();
metadata.hidden = true;
renderAddFieldMenu(fieldGroupModel());
check('menu_should_drop_metadata_once_the_block_is_shown', [shownAfterEdit, listedAfterEdit, listsMetadata()], [false, false, true]);

console.log('failures=' + failures);
if (failures) throw new Error(failures + ' add field menu check(s) failed');
