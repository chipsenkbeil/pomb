const { CARD_KIND } = require('../../static/pure.js');
var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var BoardChrome = {');
var end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('BoardChrome not found');

function store() {
  var m = {};
  return {
    getItem: function (k) { return k in m ? m[k] : null; },
    setItem: function (k, v) { m[k] = String(v); },
    removeItem: function (k) { delete m[k]; },
  };
}

function classes() {
  var set = {};
  return {
    toggle: function (name, on) { set[name] = on === undefined ? !set[name] : !!on; },
    contains: function (name) { return !!set[name]; },
  };
}

function select(value) {
  var el = {
    tagName: 'SELECT',
    value: value,
    options: [],
    children: [],
    dataset: {},
    appendChild: function (child) { el.children.push(child); el.options.push(child); return child; },
  };
  Object.defineProperty(el, 'textContent', {
    get: function () { return ''; },
    set: function () { el.children = []; el.options = []; },
  });
  return el;
}

function card(id, epicId) {
  var tag = epicId ? { dataset: { epicId: epicId } } : null;
  return {
    dataset: { taskId: id, labels: '[]', filterFields: '{}', memberIds: '', dueAt: '' },
    style: {},
    classList: classes(),
    closest: function () { return null; },
    querySelector: function (sel) { return sel === '.card-epic-tag' ? tag : null; },
  };
}

function node() {
  var el = {
    children: [], dataset: {}, style: {}, className: '', listeners: {},
    appendChild: function (child) { el.children.push(child); return child; },
    setAttribute: function () {},
    addEventListener: function (type, fn) { el.listeners[type] = fn; },
  };
  return el;
}

function find(el, cls) {
  if ((' ' + el.className + ' ').indexOf(' ' + cls + ' ') !== -1) return el;
  for (var i = 0; i < el.children.length; i++) {
    var hit = find(el.children[i], cls);
    if (hit) return hit;
  }
  return null;
}

function funnel(id, row) {
  return {
    dataset: { epicId: id },
    classList: classes(),
    attrs: {},
    setAttribute: function (k, v) { this.attrs[k] = v; },
    closest: function () { return row; },
  };
}

function load(sessionStorage, localStorage) {
  var cards = [card('1', 'e1'), card('2', 'e2'), card('3', null), card('4', 'e1')];
  var rows = [{ classList: classes() }, { classList: classes() }];
  var funnels = [funnel('e1', rows[0]), funnel('e2', rows[1])];
  var btn = { classList: classes(), addEventListener: function () {} };
  var panel = { hidden: true, listeners: {}, addEventListener: function (t, fn) { panel.listeners[t] = fn; } };
  var epicsPanel = { hidden: false };
  var els = {
    'f-user': select('all'),
    'f-color': select('all'),
    'f-date': select('all'),
    'f-epic': select(''),
    'filter-btn': btn,
    'filter-panel': panel,
    'epics-panel': epicsPanel,
    'filter-remember': { checked: false },
  };
  var document = {
    getElementById: function (id) { return els[id] || null; },
    querySelector: function () { return null; },
    querySelectorAll: function (sel) {
      if (sel === '.task-card') return cards;
      if (sel === '.epic-funnel') return funnels;
      return [];
    },
    createElement: node,
  };
  var window = { sessionStorage: sessionStorage, localStorage: localStorage, addEventListener: function () {} };
  var boardId = function () { return 'b1'; };
  var chrome = eval('(function () {' + src.slice(start, end) + '\n return BoardChrome; })()');
  chrome.syncLegendFilter = function () {};
  chrome.loadFilterLabels = function () {};
  chrome.addDateOptions = function () {};
  chrome.initFilterBookmarks = function () {};
  chrome.positionFilterPanel = function () {};
  chrome.epics = [
    { id: 'e1', name: 'One', closed: false },
    { id: 'e2', name: 'Two', closed: false },
    { id: 'e3', name: 'Three', closed: true },
  ];
  return { chrome: chrome, cards: cards, funnels: funnels, rows: rows, btn: btn, els: els, panel: panel, epicsPanel: epicsPanel };
}

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

function shown(page) {
  return page.cards.map(function (c) { return c.style.display === 'none' ? 0 : 1; });
}

var session = store(), local = store();

var page = load(session, local);
page.chrome.applyFilter();
check('no epic selected shows every card', shown(page), [1, 1, 1, 1]);
check('no epic leaves the button unlit', page.btn.classList.contains('filtering'), false);

page.funnels[0].dataset.epicId = 'e1';
page.chrome.filterOnEpic('e1');
check('a funnel hides the cards outside the epic and the cards with no pill', shown(page), [1, 0, 0, 1]);
check('a funnel lights the filter button', page.btn.classList.contains('filtering'), true);
check('the funnel select follows', page.els['f-epic'].value, 'e1');
check('the active funnel is lit', page.funnels.map(function (f) { return f.classList.contains('is-lit'); }), [true, false]);
check('the active row is lit', page.rows.map(function (r) { return r.classList.contains('is-lit'); }), [true, false]);
check('the Epics panel stays open', page.epicsPanel.hidden, false);
check('the filter panel stays closed', page.panel.hidden, true);
const marks = (p) => p.cards.map((c) => c.classList.contains('is-epic-child'));
check('the highlight marks the epic cards', marks(page), [true, false, false, true]);
page.chrome.filterOnEpic('e2');
check('another epic moves the marks', marks(page), [false, true, false, false]);
page.chrome.filterOnEpic('');
check('turning the highlight off clears the marks', marks(page), [false, false, false, false]);
page.chrome.filterOnEpic('e1');

page = load(session, local);
page.chrome.restoreFilter();
check('reload keeps the epic', [page.chrome.epicFilter, page.els['f-epic'].value], ['e1', 'e1']);
check('reload applies the epic', shown(page), [1, 0, 0, 1]);
check('reload lights the button', page.btn.classList.contains('filtering'), true);

page = load(store(), store());
var row = page.chrome.epicRow(page.chrome.epics[1], null, []);
find(row, 'epic-funnel').listeners.click();
check('clicking the rendered funnel filters to its epic', [page.chrome.epicFilter, page.els['f-epic'].value, shown(page)], ['e2', 'e2', [0, 1, 0, 0]]);
check('clicking the rendered funnel lights the button', page.btn.classList.contains('filtering'), true);
check('the Epics panel stays open after the click', page.epicsPanel.hidden, false);
check('the filter panel stays closed after the click', page.panel.hidden, true);
row = page.chrome.epicRow(page.chrome.epics[1], null, []);
check('the active epic renders lit', [find(row, 'epic-funnel').className.indexOf('is-lit') !== -1, row.className.indexOf('is-lit') !== -1], [true, true]);
find(row, 'epic-funnel').listeners.click();
check('clicking the lit funnel clears the epic', [page.chrome.epicFilter, page.els['f-epic'].value, shown(page)], ['', '', [1, 1, 1, 1]]);
check('clicking the lit funnel unlights the button', page.btn.classList.contains('filtering'), false);

var remembered = store();
page = load(store(), remembered);
page.els['filter-remember'].checked = true;
page.chrome.filterOnEpic('e1');
page = load(store(), remembered);
page.chrome.restoreFilter();
check('a remembered filter without this board\'s session brings no epic', [page.chrome.epicFilter, shown(page)], ['', [1, 1, 1, 1]]);

page = load(store(), store());
page.chrome.initFilter();
page.panel.listeners.change({ target: { id: 'f-epic', value: 'e2', classList: { contains: function () { return false; } } } });
check('the select filters the same way', shown(page), [0, 1, 0, 0]);
check('the select lights the button', page.btn.classList.contains('filtering'), true);
check('the select lights the funnel', page.funnels.map(function (f) { return f.classList.contains('is-lit'); }), [false, true]);

page = load(store(), store());
page.chrome.fillEpicSelect();
check('options are all plus the open epics', page.els['f-epic'].options.map(function (o) { return o.value; }), ['', 'e1', 'e2']);
page.chrome.setEpicFilter('e3');
page.chrome.fillEpicSelect();
check('the selected closed epic stays in the list', page.els['f-epic'].options.map(function (o) { return o.value; }), ['', 'e1', 'e2', 'e3']);
check('the selected value survives the refill', page.els['f-epic'].value, 'e3');
page.chrome.epics = [];
page.chrome.setEpicFilter('gone');
page.chrome.fillEpicSelect();
check('a restored epic whose list has not arrived keeps its option', page.els['f-epic'].options.map(function (o) { return o.value; }), ['', 'gone']);
check('the pending value is kept', page.els['f-epic'].value, 'gone');
check('the pending option waits for the list', page.els['f-epic'].options[1].textContent, 'Selected epic');
page.chrome.epicsLoaded = true;
page.chrome.fillEpicSelect();
check('a missing epic says so once the list has loaded', page.els['f-epic'].options[1].textContent, 'Epic not found');

page = load(session, local);
page.chrome.restoreFilter();
page.chrome.resetFilter();
check('reset clears the epic', [page.chrome.epicFilter, page.els['f-epic'].value], ['', '']);
check('reset shows every card', shown(page), [1, 1, 1, 1]);
check('reset unlights the button', page.btn.classList.contains('filtering'), false);
check('reset unlights the funnels', page.funnels.map(function (f) { return f.classList.contains('is-lit'); }), [false, false]);
page = load(session, local);
page.chrome.restoreFilter();
check('reset is saved', page.chrome.epicFilter, '');

if (failures) process.exit(1);
console.log('ok');
