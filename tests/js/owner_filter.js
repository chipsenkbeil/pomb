const { CARD_KIND } = require('../../static/pure.js');
let src = require('fs').readFileSync(process.argv[2], 'utf8');
let start = src.indexOf('  var BoardChrome = {');
let end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('BoardChrome not found');

function select(value) {
  return { tagName: 'SELECT', value: value, options: [], dataset: {} };
}

function card(id, owner) {
  return {
    dataset: { taskId: id, owner: owner, labels: '[]', filterFields: '{}', memberIds: '', dueAt: '' },
    style: {},
    classList: { toggle: function () {} },
    querySelector: function () { return null; },
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

function load(sessionStorage) {
  let cards = [card('1', 'user:me'), card('2', 'role:builder'), card('3', ''), card('4', 'user:ann'), card('5', 'agent:solo')];
  let options = [
    ['Me', 'user:me'], ['Ann', 'user:ann'], ['builder (role)', 'role:builder'], ['No owner', 'none'],
  ].map(function (pair) { return { value: pair[0], dataset: { owner: pair[1] } }; });
  let input = { value: '' };
  let els = {
    'f-user': select('all'),
    'f-color': select('all'),
    'f-date': select('all'),
    'f-owner': input,
    'f-label-mode': select('and'),
    'filter-btn': { classList: { toggle: function () {} } },
    'filter-reset': { hidden: true },
    'filter-reset-panel': { hidden: true },
    'filter-panel': { hidden: true },
  };
  let document = {
    getElementById: function (id) { return els[id] || null; },
    querySelector: function () { return null; },
    querySelectorAll: function (sel) {
      if (sel === '.task-card') return cards;
      if (sel === '#f-owner-list option') return options;
      return [];
    },
  };
  let window = { sessionStorage: sessionStorage, localStorage: store() };
  let boardId = function () { return 'b1'; };
  let preamble = 'let cssEscape = function (s) { return s; }; let cardEstimateMinutes = function () { return 0; };' +
    'let setSumBadge = function () {}; let syncWarningLine = function () {}; let toast = function () {};';
  let chrome = eval('(function () {' + preamble + src.slice(start, end) + '\n return BoardChrome; })()');
  return { chrome: chrome, cards: cards, input: input };
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
let page = load(session);
page.chrome.applyFilter();
check('no owner chosen shows every card', shown(page), ['1', '2', '3', '4', '5']);
check('an empty field is not filtering', page.chrome.isFiltering(), false);

page.input.value = 'me';
page.chrome.applyFilter();
check('a person shows the cards they own', shown(page), ['1']);
check('a chosen owner is filtering', page.chrome.isFiltering(), true);

page.input.value = 'builder (role)';
page.chrome.applyFilter();
check('a role shows the cards its tokens own', shown(page), ['2']);

page.input.value = 'No owner';
page.chrome.applyFilter();
check('No owner shows the free cards', shown(page), ['3']);

page.input.value = 'nobody typed this';
page.chrome.applyFilter();
check('text that names no choice filters nothing', shown(page), ['1', '2', '3', '4', '5']);

page.input.value = 'Ann';
page.chrome.maybeSaveFilter();
page = load(session);
page.chrome.restoreFilter();
check('a restored filter shows the same owner', page.input.value, 'Ann');
check('a restored filter hides the other cards', shown(page), ['4']);

page.chrome.resetFilter();
check('reset clears the owner', page.input.value, '');
check('reset shows every card', shown(page), ['1', '2', '3', '4', '5']);

if (failures) process.exit(1);
console.log('owner filter ok');
