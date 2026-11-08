const { CARD_KIND } = require('../../static/pure.js');
const src = require('fs').readFileSync(process.argv[2], 'utf8');
const start = src.indexOf('  var BoardChrome = {');
const end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('BoardChrome not found');

const CARD = 100;
const FOLD = 50;
const VIEW = 300;

const store = () => {
  const m = {};
  return {
    getItem: (k) => (k in m ? m[k] : null),
    setItem: (k, v) => { m[k] = String(v); },
    removeItem: (k) => { delete m[k]; },
  };
};

const select = (value) => {
  const options = ['all', 'red', 'blue', ''].map((v) => ({ value: v }));
  return { tagName: 'SELECT', value, options, children: [], dataset: {} };
};

function load() {
  let raw = 0;
  const list = { dataset: { columnId: 'c1' }, children: [] };
  const isFold = (el) => el.className === 'filter-fold';
  const visible = () => list.children.filter((el) => (el.isCard || isFold(el)) && !el.hidden && el.style.display !== 'none');
  const height = (el) => (el.isCard ? CARD : FOLD);
  const limit = () => Math.max(0, visible().reduce((sum, el) => sum + height(el), 0) - VIEW);
  Object.defineProperty(list, 'scrollTop', {
    get: () => { raw = Math.min(raw, limit()); return raw; },
    set: (v) => { raw = Math.max(0, Math.min(v, limit())); },
  });
  const rect = (el) => {
    const above = visible().slice(0, visible().indexOf(el)).reduce((sum, e) => sum + height(e), 0);
    const top = above - list.scrollTop;
    return { top, bottom: top + height(el) };
  };
  list.getBoundingClientRect = () => ({ top: 0, bottom: VIEW });
  list.contains = (el) => list.children.indexOf(el) !== -1;
  list.querySelectorAll = (sel) => {
    if (sel === '.task-card') return list.children.filter((el) => el.isCard);
    if (sel === '.task-card, .filter-fold') return list.children.filter((el) => el.isCard || isFold(el));
    return [];
  };
  list.insertBefore = (row, ref) => { list.children.splice(list.children.indexOf(ref), 0, row); };
  list.querySelector = () => null;
  const card = (i) => {
    const el = {
      isCard: true,
      dataset: { taskId: String(i), colorValue: i % 2 === 0 ? 'red' : 'blue', labels: '[]', filterFields: '{}', memberIds: '', dueAt: '' },
      style: new Proxy({}, {
        set: (target, key, value) => {
          target[key] = value;
          list.scrollTop = list.scrollTop;
          return true;
        },
      }),
      hidden: false,
      classList: { contains: (name) => name === 'task-card', toggle: () => {} },
      closest: (sel) => (sel === '.task-list' ? list : null),
      querySelector: () => null,
      getBoundingClientRect: () => rect(el),
    };
    return el;
  };
  const cards = [];
  for (let i = 0; i < 10; i++) cards.push(card(i));
  list.children = cards.slice();
  const els = {
    'f-user': select('all'),
    'f-color': select('all'),
    'f-date': select('all'),
    'f-epic': select(''),
    'filter-btn': { classList: { toggle: () => {} }, addEventListener: () => {} },
    'filter-remember': { checked: false },
  };
  const document = {
    getElementById: (id) => els[id] || null,
    querySelector: (sel) => (sel === 'input[name="f-fold"]:checked' ? { value: 'hide' } : null),
    createElement: () => {
      const row = { className: '', style: {}, textContent: '' };
      row.remove = () => { list.children.splice(list.children.indexOf(row), 1); };
      row.getBoundingClientRect = () => rect(row);
      return row;
    },
    querySelectorAll: (sel) => {
      if (sel === '.task-card') return list.children.filter((el) => el.isCard);
      if (sel === '.task-list') return [list];
      if (sel === '.filter-fold') return list.children.filter(isFold);
      return [];
    },
  };
  const window = { sessionStorage: store(), localStorage: store(), addEventListener: () => {} };
  const boardId = () => 'b1';
  const chrome = eval('(function () {' + src.slice(start, end) + '\n return BoardChrome; })()');
  chrome.syncLegendFilter = () => {};
  return { chrome, cards, list, els };
}

let failures = 0;
const check = (name, got, want) => {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
};

let page = load();
page.list.scrollTop = 450;
page.els['f-color'].value = 'red';
page.chrome.applyFilter();
check('filtering clamps the column', page.list.scrollTop, 200);
page.els['f-color'].value = 'all';
page.chrome.applyFilter();
check('clearing returns an unscrolled column to its card and offset', page.list.scrollTop, 450);

page = load();
page.list.scrollTop = 450;
page.els['f-color'].value = 'red';
page.chrome.applyFilter();
page.list.scrollTop = 100;
page.els['f-color'].value = 'all';
page.chrome.applyFilter();
check('clearing keeps the topmost card of a column scrolled while filtered', page.list.scrollTop, 200);

page = load();
page.list.scrollTop = 450;
page.els['f-color'].value = 'red';
page.chrome.applyFilter();
page.els['f-user'].value = 'all';
page.chrome.applyFilter();
page.els['f-color'].value = 'all';
page.chrome.applyFilter();
check('a second filter change keeps the pre-filter place', page.list.scrollTop, 450);

page = load();
page.list.scrollTop = 450;
page.els['f-color'].value = 'red';
page.chrome.applyFilter();
page.els['f-color'].value = 'blue';
page.chrome.applyFilter();
page.list.scrollTop = 150;
page.els['f-color'].value = 'all';
page.chrome.applyFilter();
check('a scrolled column stays on its filtered card after a changed filter', page.list.scrollTop, 350);

page = load();
page.list.scrollTop = 450;
page.chrome.filterOnEpic('3');
page.chrome.filterOnEpic('');
check('the epic funnel restores the place', page.list.scrollTop, 450);

page = load();
page.list.scrollTop = 450;
page.els['f-color'].value = 'red';
page.chrome.applyFilter();
page.chrome.resetFilter();
check('Clear restores the place', page.list.scrollTop, 450);

page = load();
page.list.scrollTop = 450;
page.els['f-color'].value = 'blue';
page.chrome.applyFilter();
page.list.children.splice(page.list.children.indexOf(page.cards[4]), 1);
page.els['f-color'].value = 'all';
page.chrome.applyFilter();
check('a missing anchor falls back to the nearest earlier card', page.list.scrollTop, 300);

page = load();
page.list.scrollTop = 450;
page.chrome.holdScroll(page.list, () => {
  page.cards[0].style.display = 'none';
  page.cards[1].style.display = 'none';
});
check('a held change keeps the place', page.list.scrollTop, 250);

page = load();
page.chrome.filterOnEpic('7');
const fold = page.list.children.find((el) => el.className === 'filter-fold');
page.chrome.openFold(fold);
check('opening a fold at the top leaves the column at the top', page.list.scrollTop, 0);
page.chrome.closeFold();
check('closing a fold at the top leaves the column at the top', page.list.scrollTop, 0);

if (failures) process.exit(1);
console.log('ok');
