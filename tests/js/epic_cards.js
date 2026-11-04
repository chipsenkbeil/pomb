const src = require('fs').readFileSync(process.argv[2], 'utf8');
const slice = (from, to) => {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) throw new Error('not found: ' + from);
  return src.slice(start, end);
};

const classSet = () => {
  const names = new Set();
  return {
    add: (name) => names.add(name),
    remove: (name) => names.delete(name),
    toggle: (name, on) => { if (on) names.add(name); else names.delete(name); },
    contains: (name) => names.has(name),
  };
};

const makeCard = (id, extra) => {
  const card = {
    dataset: { taskId: id },
    hidden: false,
    style: {},
    classList: classSet(),
    querySelector: () => null,
    scrolls: [],
    offsetWidth: 1,
    getClientRects: () => [1],
    scrollIntoView(options) { card.scrolls.push(options); },
    after(other) { card.list.insertAfter(other, card); },
    ...extra,
  };
  return card;
};

const makeList = (cards) => {
  const list = {
    order: [...cards],
    querySelectorAll: () => list.order,
    insertBefore(card, before) {
      list.order = list.order.filter((el) => el !== card);
      list.order.splice(list.order.indexOf(before), 0, card);
    },
    insertAfter(card, anchor) {
      list.order = list.order.filter((el) => el !== card);
      list.order.splice(list.order.indexOf(anchor) + 1, 0, card);
    },
    appendChild(card) { list.order = list.order.filter((el) => el !== card); list.order.push(card); },
  };
  cards.forEach((card) => { card.list = list; });
  return list;
};

let failures = 0;
const check = (name, got, want) => {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
};

const toasts = [];
const toastRefusal = (res, title) => {
  toasts.push([title, res.message]);
  return Promise.resolve(null);
};
const window = {};
let refolds = 0;
window.BoardChrome = { refold: () => { refolds++; } };
const BoardChrome = window.BoardChrome;
const refuse = eval('(function () {' + slice('  const putCardBack', '  // Note: htmx inserts') + '\n return refuseMove; })()');

const start = src.indexOf('  var BoardChrome = {');
const end = src.indexOf('\n  };', start) + '\n  };'.length;
const toastsShown = [];
const toast = (message, detail) => { toastsShown.push([message, detail]); };
const cards = [];
const document = {
  querySelectorAll: (sel) => {
    if (sel === '.task-card') return cards;
    if (sel === '.card-epic-toggle') return cards.filter((c) => c.toggle).map((c) => c.toggle);
    return [];
  },
  createElement: () => {
    const el = { dataset: {}, attrs: {}, listeners: {}, className: '', addEventListener: (t, fn) => { el.listeners[t] = fn; }, setAttribute: (k, v) => { el.attrs[k] = v; } };
    return el;
  },
};
const timers = [];
const winLocal = { setTimeout: (fn) => { timers.push(fn); } };
const chrome = eval('(function () { const window = winLocal;' + src.slice(start, end) + '\n return BoardChrome; })()');

(async () => {
  const a = makeCard('a');
  const epicCard = makeCard('e', { dataset: { taskId: 'e', kind: 'epic' } });
  const c = makeCard('c');
  const list = makeList([a, c]);
  epicCard.list = list;
  await refuse({ message: 'the epic still has open children (2), so it cannot enter a WIP-limited or Done column' }, epicCard, list, 1);
  check('a refused epic drag shows the server reason', toasts, [['Could not move task.', 'the epic still has open children (2), so it cannot enter a WIP-limited or Done column']]);
  check('a refused epic drag puts the card back at its old place', list.order.map((el) => el.dataset.taskId), ['a', 'e', 'c']);
  check('a refused drag refolds the board', refolds, 1);

  await refuse({ message: 'x' }, epicCard, list, 5);
  check('a refused drag past the end puts the card last', list.order.map((el) => el.dataset.taskId), ['a', 'c', 'e']);

  const button = chrome.locateButton({ id: 'e' });
  check('the locate button is labelled Show on board', [button.attrs['aria-label'], button.title], ['Show on board', 'Show on board']);
  check('the locate button names its epic', button.dataset.epicId, 'e');
  check('the locate button sits beside the funnel in the row', button.className.includes('epic-locate'), true);

  cards.push(a, epicCard);
  chrome.locateEpic('e');
  check('locating scrolls to the epic card', epicCard.scrolls.length, 1);
  check('locating rings the card', epicCard.classList.contains('card-located'), true);
  timers.forEach((fn) => fn());
  check('the ring clears after a moment', epicCard.classList.contains('card-located'), false);

  epicCard.style.display = 'none';
  chrome.locateEpic('e');
  check('a filtered-out card is not ringed', epicCard.classList.contains('card-located'), false);
  check('a filtered-out card says so', toastsShown.length, 1);
  chrome.locateEpic('missing');
  check('an epic with no card says so', toastsShown.length, 2);

  const lit = makeCard('e2', { dataset: { taskId: 'e2', kind: 'epic' } });
  const plain = makeCard('p');
  lit.toggle = { dataset: { epicId: 'e2' }, attrs: {}, setAttribute(k, v) { this.attrs[k] = v; }, closest: () => lit };
  plain.toggle = { dataset: { epicId: 'e3' }, attrs: {}, setAttribute(k, v) { this.attrs[k] = v; }, closest: () => plain };
  cards.push(lit, plain);
  chrome.epicFilter = 'e2';
  chrome.syncEpicFunnels();
  check('the chosen epic card is lit with its toggle pressed', [lit.classList.contains('is-lit'), lit.toggle.attrs['aria-pressed']], [true, 'true']);
  check('another epic card stays unlit', [plain.classList.contains('is-lit'), plain.toggle.attrs['aria-pressed']], [false, 'false']);

  if (failures) process.exit(1);
  console.log('epic cards ok');
})();
