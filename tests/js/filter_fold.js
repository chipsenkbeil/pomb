var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var BoardChrome = {');
var end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('BoardChrome not found');

function store() {
  var m = {};
  return {
    getItem: function (k) { return k in m ? m[k] : null; },
    setItem: function (k, v) { m[k] = String(v); },
    removeItem: function (k) { delete m[k]; }
  };
}

function select(value) {
  var options = ['all', 'red', 'blue', ''].map(function (v) { return { value: v }; });
  return { tagName: 'SELECT', value: value, options: options, children: [], dataset: {} };
}

function classSet(names) {
  return { contains: function (name) { return names.indexOf(name) !== -1; } };
}

function load(sessionStorage, localStorage) {
  var mode = 'hide';
  var list = { dataset: { columnId: 'c1' }, children: [] };
  list.insertBefore = function (row, ref) {
    row.parent = list;
    list.children.splice(list.children.indexOf(ref), 0, row);
  };
  function card(id, color) {
    return {
      dataset: { taskId: id, colorValue: color, labels: '[]', filterFields: '{}', memberIds: '', dueAt: '' },
      style: {},
      hidden: false,
      classList: classSet(['task-card']),
      closest: function (sel) { return sel === '.task-list' ? list : null; },
      querySelector: function () { return null; },
      remove: function () { list.children.splice(list.children.indexOf(this), 1); }
    };
  }
  var cards = [card('1', 'red'), card('2', 'blue'), card('3', 'blue'), card('4', 'red'), card('5', 'blue'), card('6', 'blue'), card('7', 'blue')];
  list.children = cards.slice();
  var els = {
    'f-user': select('all'),
    'f-color': select('all'),
    'f-date': select('all'),
    'f-epic': select(''),
    'filter-btn': { classList: { toggle: function () {} }, addEventListener: function () {} },
    'filter-remember': { checked: false }
  };
  function folds() {
    return list.children.filter(function (el) { return el.className === 'filter-fold'; });
  }
  var document = {
    getElementById: function (id) { return els[id] || null; },
    querySelector: function (sel) {
      if (sel === 'input[name="f-fold"]:checked') return { value: mode };
      var m = /^input\[name="f-fold"\]\[value="(\w+)"\]$/.exec(sel);
      if (!m) return null;
      var radio = {};
      Object.defineProperty(radio, 'checked', { set: function () { mode = m[1]; } });
      return radio;
    },
    querySelectorAll: function (sel) {
      if (sel === '.task-card') return cards.filter(function (c) { return list.children.indexOf(c) !== -1; });
      if (sel === '.task-list') return [list];
      if (sel === '.filter-fold') return folds();
      return [];
    },
    createElement: function () {
      var row = { className: '', style: {}, textContent: '', classList: classSet([]) };
      row.remove = function () { list.children.splice(list.children.indexOf(row), 1); };
      row.closest = function (sel) { return sel === '.filter-fold' ? row : null; };
      return row;
    }
  };
  var window = { sessionStorage: sessionStorage, localStorage: localStorage, addEventListener: function () {} };
  var boardId = function () { return 'b1'; };
  var chrome = eval('(function () {' + src.slice(start, end) + '\n return BoardChrome; })()');
  chrome.syncLegendFilter = function () {};
  function label() {
    return { dataset: { taskId: 'L' }, style: {}, classList: classSet(['done-group-label']) };
  }
  return {
    label: label,
    chrome: chrome, cards: cards, card: card, els: els, list: list, folds: folds,
    mode: function () { return mode; },
    setMode: function (m) { mode = m; }
  };
}

function over(page, where) {
  return {
    closest: function (sel) {
      if (sel === '.task-list') return where.list || null;
      if (sel === '.task-card') return where.card || null;
      if (sel === '.filter-fold') return where.fold || null;
      return null;
    }
  };
}

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

function shown(page) {
  return page.cards.map(function (c) { return c.style.display === 'none' ? 0 : 1; });
}

function layout(page) {
  return page.list.children.map(function (el) {
    return el.className === 'filter-fold' ? el.textContent : el.dataset.taskId;
  });
}

var page = load(store(), store());
page.els['f-color'].value = 'red';
page.chrome.applyFilter();
check('hide mode hides without fold rows', [shown(page), page.folds().length], [[1, 0, 0, 1, 0, 0, 0], 0]);

page.setMode('collapse');
page.chrome.applyFilter();
check('collapse mode puts one fold row in place of each run', layout(page), ['1', '2 other cards', '2', '3', '4', '3 other cards', '5', '6', '7']);
check('collapse mode still hides the cards', shown(page), [1, 0, 0, 1, 0, 0, 0]);
check('rebuilding does not stack fold rows', (page.chrome.applyFilter(), page.folds().length), 2);

page.els['f-color'].value = 'blue';
page.chrome.applyFilter();
check('a changed filter rebuilds the folds', layout(page), ['1 other card', '1', '2', '3', '1 other card', '4', '5', '6', '7']);

page = load(store(), store());
page.els['f-color'].value = 'red';
page.setMode('collapse');
page.chrome.applyFilter();
var first = page.folds()[0];
page.chrome.foldDragOver(over(page, { fold: first, list: page.list }));
check('dragging over a fold shows its cards', shown(page), [1, 1, 1, 1, 0, 0, 0]);
check('dragging over a fold hides the row itself', first.style.display, 'none');
page.chrome.foldDragOver(over(page, { card: page.cards[1], list: page.list }));
check('staying over the opened cards keeps the fold open', shown(page), [1, 1, 1, 1, 0, 0, 0]);
page.chrome.foldDragOver(over(page, { list: page.list }));
check('the gap between opened cards keeps the fold open', shown(page), [1, 1, 1, 1, 0, 0, 0]);
page.chrome.foldDragOver(over(page, { list: {} }));
check('another list refolds', shown(page), [1, 0, 0, 1, 0, 0, 0]);
page.chrome.foldDragOver(over(page, { fold: first, list: page.list }));
page.chrome.foldDragOver(over(page, { card: page.cards[3], list: page.list }));
check('leaving the fold refolds it', [shown(page), first.style.display], [[1, 0, 0, 1, 0, 0, 0], '']);

page.chrome.foldDragOver(over(page, { fold: first, list: page.list }));
page.chrome.unfold();
check('unfold removes every fold row and keeps the hidden cards hidden', [page.folds().length, shown(page)], [0, [1, 0, 0, 1, 0, 0, 0]]);
check('with the fold rows gone the list holds only cards', layout(page), ['1', '2', '3', '4', '5', '6', '7']);
page.chrome.refold();
check('refold brings the folds back without re-evaluating', page.folds().length, 2);

function collapsed() {
  var p = load(store(), store());
  p.els['f-color'].value = 'red';
  p.setMode('collapse');
  p.chrome.applyFilter();
  return p;
}

page = collapsed();
page.cards[3].remove();
page.chrome.refold();
check('removing a visible card between two runs merges them', layout(page), ['1', '5 other cards', '2', '3', '5', '6', '7']);

page = collapsed();
page.cards[1].remove();
page.cards[2].remove();
page.chrome.refold();
check('removing a whole run leaves no empty fold', layout(page), ['1', '4', '3 other cards', '5', '6', '7']);

page = collapsed();
page.cards[4].remove();
page.cards[5].remove();
page.cards[6].remove();
page.chrome.refold();
check('removing the last card of a run drops its fold', layout(page), ['1', '2 other cards', '2', '3', '4']);

page = collapsed();
page.list.children.splice(page.list.children.indexOf(page.cards[2]), 0, page.label());
page.chrome.refold();
check('a divider ends a run so a fold never spans groups', layout(page), ['1', '1 other card', '2', 'L', '1 other card', '3', '4', '3 other cards', '5', '6', '7']);

page = collapsed();
page.cards[4].hidden = true;
page.chrome.refold();
check('cards behind Load more stay out of runs and counts', layout(page), ['1', '2 other cards', '2', '3', '4', '5', '2 other cards', '6', '7']);

var saved = store();
page = load(saved, store());
page.setMode('collapse');
page.chrome.maybeSaveFilter();
check('collapse is saved with the filter state', JSON.parse(saved.getItem('pomb-filter-session:b1')).fold, 'collapse');

page = load(store(), store());
page.els['f-color'].value = 'red';
page.setMode('collapse');
page.chrome.applyFilter();

page.chrome.resetFilter();
check('reset returns the switch to hide', page.mode(), 'hide');
check('reset shows every card with no folds', [shown(page), page.folds().length], [[1, 1, 1, 1, 1, 1, 1], 0]);

var session = store();
page = load(session, store());
page.els['f-color'].value = 'red';
page.setMode('collapse');
page.chrome.maybeSaveFilter();
page = load(session, store());
page.els['f-color'].value = 'red';
page.chrome.restoreFilter();
check('a reload restores collapse mode', page.mode(), 'collapse');

session = store();
session.setItem('pomb-filter-session:b1', JSON.stringify({ user: 'all', color: 'all', date: 'all', labels: [], fields: {} }));
page = load(session, store());
page.setMode('collapse');
page.chrome.restoreFilter();
check('saved state without the key means hide', page.mode(), 'hide');

page = collapsed();
var endStart = src.indexOf('onEnd: function (evt) {');
var endStop = src.indexOf('var cards = Array.prototype.slice.call(toList', endStart);
if (endStart < 0 || endStop < endStart) throw new Error('onEnd not found');
var dragging = true;
var dragGroup = null;
var BoardChrome = page.chrome;
var window = { setTimeout: function () {}, BoardChrome: BoardChrome };
var markDoneTarget = function () {};
var droppedOnCollapsedStrip = function () { return true; };
var toast = function () {};
var onEnd = eval('(function (evt) {' + src.slice(endStart + 'onEnd: function (evt) {'.length, endStop) + '})');
var strip = page.list;
strip.querySelectorAll = function () {
  return strip.children.filter(function (el) { return el.className !== 'filter-fold'; });
};
strip.insertBefore = function (el, ref) {
  var at = strip.children.indexOf(el);
  if (at !== -1) strip.children.splice(at, 1);
  strip.children.splice(strip.children.indexOf(ref), 0, el);
};
var moved = page.cards[3];
moved.after = function (el) {
  var at = strip.children.indexOf(el);
  if (at !== -1) strip.children.splice(at, 1);
  strip.children.splice(strip.children.indexOf(this) + 1, 0, el);
};
strip.children.splice(strip.children.indexOf(moved), 1);
strip.children.push(moved);
onEnd({ item: moved, to: strip, from: strip, oldIndex: 5, oldDraggableIndex: 3 });
check('a drop on a collapsed strip restores the card by its index among cards, not fold rows', strip.children.map(function (el) { return el.dataset.taskId; }), ['1', '2', '3', '4', '5', '6', '7']);

if (failures) { console.log(failures + ' failed'); process.exit(1); }
console.log('ok');
