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
  const timers = [];
  let now = 0;
  const window = {
    sessionStorage: store(),
    localStorage: store(),
    addEventListener: () => {},
    setTimeout: (fn, ms) => { timers.push({ fn, at: now + ms }); return timers.length; },
    clearTimeout: (id) => { if (id) timers[id - 1] = null; },
  };
  const tick = (ms) => {
    const until = now + ms;
    for (;;) {
      const due = timers.filter((t) => t && t.at <= until).sort((a, b) => a.at - b.at)[0];
      if (!due) break;
      timers[timers.indexOf(due)] = null;
      now = due.at;
      due.fn();
    }
    now = until;
  };
  const boardId = () => 'b1';
  const chrome = eval('(function () {' + src.slice(start, end) + '\n return BoardChrome; })()');
  chrome.syncLegendFilter = () => {};
  return { chrome, cards, list, els, tick };
}

let failures = 0;
const check = (name, got, want) => {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
};

const folded = () => {
  const page = load();
  page.chrome.filterOnEpic('7');
  const fold = page.list.children.find((el) => el.className === 'filter-fold');
  const onFold = { closest: (sel) => (sel === '.filter-fold' ? fold : null) };
  const away = { closest: () => null };
  const inRun = {
    closest: (sel) => (sel === '.task-list' ? page.list : sel === '.task-card' ? fold._run[0] : null),
  };
  return { page, fold, onFold, away, inRun };
};
const isOpen = (fold) => fold.style.display === 'none';

let t = folded();
check('the dwell constants carry their unit', [t.page.chrome.OPEN_DWELL_MS, t.page.chrome.CLOSE_GRACE_MS], [400, 150]);
t.page.chrome.foldDragOver(t.onFold);
check('a fold stays closed on first contact', isOpen(t.fold), false);
t.page.tick(399);
check('a fold stays closed before the dwell', isOpen(t.fold), false);
t.page.chrome.foldDragOver(t.onFold);
t.page.tick(1);
check('a fold opens once the pointer has rested for the dwell', isOpen(t.fold), true);
check('a dwell open keeps the column at the top', t.page.list.scrollTop, 0);

t = folded();
t.page.chrome.foldDragOver(t.onFold);
t.page.tick(200);
t.page.chrome.foldDragOver(t.away);
t.page.tick(1000);
check('leaving before the dwell leaves the fold closed', isOpen(t.fold), false);

t = folded();
t.page.chrome.foldDragOver(t.onFold);
t.page.tick(300);
t.page.chrome.foldDragOver(t.onFold);
t.page.tick(100);
check('repeated dragover does not restart the dwell', isOpen(t.fold), true);

t = folded();
t.page.chrome.foldDragOver(t.onFold);
t.page.tick(400);
t.page.chrome.foldDragOver(t.away);
t.page.tick(149);
check('an open fold survives the close grace', isOpen(t.fold), true);
t.page.tick(1);
check('an open fold closes after the pointer leaves', isOpen(t.fold), false);
check('the closed fold row shows again', t.fold.style.display, '');

t = folded();
t.page.chrome.foldDragOver(t.onFold);
t.page.tick(400);
t.page.chrome.foldDragOver(t.away);
t.page.tick(100);
t.page.chrome.foldDragOver(t.inRun);
t.page.tick(1000);
check('returning to the opened cards cancels the close', isOpen(t.fold), true);

t = folded();
t.page.chrome.foldDragOver(t.onFold);
t.page.chrome.unfold();
t.page.tick(1000);
check('drag end cancels a pending open', t.page.chrome.openedFold, null);

t = folded();
t.page.chrome.foldDragOver(t.onFold);
t.page.tick(400);
t.page.chrome.foldDragOver(t.away);
t.page.chrome.unfold();
t.page.tick(1000);
check('drag end leaves no timer to close or open a fold later', t.page.chrome.openedFold, null);

t = folded();
t.page.chrome.foldDragOver(t.onFold);
t.page.chrome.refold();
t.page.tick(1000);
check('a refold cancels a pending open', t.page.chrome.openedFold, null);

if (failures) process.exit(1);
console.log('ok');
