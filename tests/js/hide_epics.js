const { CARD_KIND } = require('../../static/pure.js');
let src = require('fs').readFileSync(process.argv[2], 'utf8');
let start = src.indexOf('  var BoardChrome = {');
let end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('BoardChrome not found');

function select(value) {
  return { tagName: 'SELECT', value: value, options: [{ value: '' }, { value: 'e1' }], dataset: {}, appendChild: function () {} };
}

function card(id, kind, parent) {
  let tag = parent ? { dataset: { epicId: parent } } : null;
  return {
    dataset: { taskId: id, kind: kind, labels: '[]', filterFields: '{}', memberIds: '', dueAt: '' },
    style: {},
    classList: { toggle: function () {} },
    querySelector: function (sel) { return sel === '.card-epic-tag' ? tag : null; },
    closest: function () { return null; },
  };
}

function store() {
  let m = {};
  return {
    getItem: function (k) { return k in m ? m[k] : null; },
    setItem: function (k, v) { m[k] = String(v); },
    removeItem: function (k) { delete m[k]; },
  };
}

function load(sessionStorage, localStorage) {
  let cards = [card('e1', 'epic'), card('e2', 'epic'), card('1', '', 'e1'), card('2', '', 'e2'), card('3', '')];
  let els = {
    'f-user': select('all'),
    'f-color': select('all'),
    'f-date': select('all'),
    'f-epic': select(''),
    'f-label-mode': select('and'),
    'f-hide-epics': { checked: false },
    'filter-remember': { checked: false },
    'filter-btn': { classList: { toggle: function () {} } },
    'filter-reset': { hidden: true },
    'filter-reset-panel': { hidden: true },
    'filter-panel': { hidden: true },
  };
  let document = {
    getElementById: function (id) { return els[id] || null; },
    createElement: function () { return {}; },
    querySelector: function () { return null; },
    querySelectorAll: function (sel) { return sel === '.task-card' ? cards : []; },
  };
  let window = { sessionStorage: sessionStorage, localStorage: localStorage };
  let boardId = function () { return 'b1'; };
  let preamble = 'let cssEscape = function (s) { return s; }; let cardEstimateMinutes = function () { return 0; };' +
    'let setSumBadge = function () {}; let syncWarningLine = function () {}; let toast = function () {};';
  let chrome = eval('(function () {' + preamble + src.slice(start, end) + '\n return BoardChrome; })()');
  return { chrome: chrome, cards: cards, els: els };
}

let failures = 0;
function check(name, got, want) {
  let g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function shown(page) {
  return page.cards.filter(function (c) { return c.style.display !== 'none'; }).map(function (c) { return c.dataset.taskId; });
}

let session = store();
let local = store();
let page = load(session, local);
page.chrome.applyFilter();
check('epic cards show by default', shown(page), ['e1', 'e2', '1', '2', '3']);
check('the switch off is not filtering', page.chrome.isFiltering(), false);

page.els['f-hide-epics'].checked = true;
page.chrome.applyFilter();
check('the switch hides every epic card', shown(page), ['1', '2', '3']);
check('the switch is an active filter', page.chrome.isFiltering(), true);
check('the reset link shows while it is on', page.els['filter-reset'].hidden, false);

page.chrome.filterOnEpic('e1');
check('the epic select still filters to children', shown(page), ['1']);

page.chrome.filterOnEpic('');
page.els['filter-remember'].checked = true;
page.chrome.maybeSaveFilter();
page = load(store(), local);
page.chrome.restoreFilter();
check('a reload keeps the switch on', page.els['f-hide-epics'].checked, true);
check('a reload keeps epic cards hidden', shown(page), ['1', '2', '3']);

page.chrome.resetFilter();
check('reset turns the switch off', page.els['f-hide-epics'].checked, false);
check('reset shows the epic cards', shown(page), ['e1', 'e2', '1', '2', '3']);
check('reset hides the reset link', page.els['filter-reset'].hidden, true);

if (failures) process.exit(1);
console.log('hide epics ok');
