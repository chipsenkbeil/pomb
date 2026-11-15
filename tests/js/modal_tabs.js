const src = require('fs').readFileSync(process.argv[2], 'utf8');
const from = src.indexOf('  var modalSubviewOpen = false;');
const to = src.indexOf('\n  var LINK_PICKER_DEBOUNCE_MS');
if (from < 0 || to < from) throw new Error('modal tab code not found');
const tabCode = src.slice(from, to);

const TAB_NAMES = ['description', 'children', 'subtasks', 'comments', 'relations', 'files', 'history'];
let document = null;

const load = (dom) => {
  document = dom;
  const modalTaskId = () => null;
  eval(tabCode);
  return { selectModalTab, wireModalTabs };
};

const tabElement = (name, hidden, focused) => ({
  name,
  hidden,
  tabIndex: -1,
  attrs: { 'data-tm-tab': name },
  listeners: {},
  getAttribute(attr) { return this.attrs[attr] === undefined ? null : this.attrs[attr]; },
  setAttribute(attr, value) { this.attrs[attr] = String(value); },
  addEventListener(type, fn) { this.listeners[type] = fn; },
  focus() { focused.push(name); },
});

const makeDom = () => {
  const focused = [];
  const tabs = TAB_NAMES.map((name) => tabElement(name, name === 'subtasks', focused));
  const panels = {};
  TAB_NAMES.forEach((name) => { panels[`tm-panel-${name}`] = { hidden: true }; });
  const byId = {};
  tabs.forEach((tab) => { byId[`tm-tab-${tab.name}`] = tab; });
  return {
    tabs,
    panels,
    focused,
    querySelectorAll: (selector) => (selector === '#modal-root [data-tm-tab]' ? tabs : []),
    getElementById: (id) => byId[id] || panels[id] || null,
  };
};

const press = (dom, name, key) => {
  const tab = dom.tabs.filter((t) => t.name === name)[0];
  dom.focused.length = 0;
  tab.listeners.keydown({ key, metaKey: false, ctrlKey: false, altKey: false, preventDefault: () => {} });
  return dom.focused.slice();
};

let failures = 0;
const check = (name, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got)));
};

const selectDom = makeDom();
const selectUi = load(selectDom);
selectUi.wireModalTabs();
const finalCheck = selectDom.getElementById('tm-tab-subtasks');
check('the Final check tab starts hidden', finalCheck.hidden, true);
selectUi.selectModalTab('subtasks');
check('selecting the hidden Final check tab unhides it', finalCheck.hidden, false);
check('selecting the hidden Final check tab marks it selected', finalCheck.getAttribute('aria-selected'), 'true');
check('selecting the hidden Final check tab makes it the focusable tab', finalCheck.tabIndex, 0);
check('selecting the hidden Final check tab shows its panel', selectDom.panels['tm-panel-subtasks'].hidden, false);
check('selecting the hidden Final check tab deselects the others', selectDom.tabs
  .filter((t) => t !== finalCheck)
  .every((t) => t.getAttribute('aria-selected') === 'false'), true);

const keyDom = makeDom();
const keyUi = load(keyDom);
keyUi.wireModalTabs();
const keyFinalCheck = keyDom.getElementById('tm-tab-subtasks');
const landings = [
  ['ArrowRight from Children skips the hidden Final check tab', press(keyDom, 'children', 'ArrowRight'), ['comments']],
  ['ArrowLeft from Comments skips the hidden Final check tab', press(keyDom, 'comments', 'ArrowLeft'), ['children']],
  ['ArrowLeft from Children goes to Description', press(keyDom, 'children', 'ArrowLeft'), ['description']],
  ['ArrowLeft from Description wraps to History', press(keyDom, 'description', 'ArrowLeft'), ['history']],
  ['ArrowRight from History wraps to Description', press(keyDom, 'history', 'ArrowRight'), ['description']],
  ['End from Comments lands on History', press(keyDom, 'comments', 'End'), ['history']],
  ['Home from Comments lands on Description', press(keyDom, 'comments', 'Home'), ['description']],
];
landings.forEach((c) => check(c[0], c[1], c[2]));
check('no arrow, Home or End key focuses the hidden Final check tab', landings.some((c) => c[1].includes('subtasks')), false);
check('the arrow and Home/End keys leave the Final check tab hidden', keyFinalCheck.hidden, true);

if (failures) process.exit(1);
