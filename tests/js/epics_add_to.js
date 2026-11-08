var src = require('fs').readFileSync(process.argv[2], 'utf8');
function fn(start, indent) {
  var pad = indent || '  ';
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n' + pad + '}\n', from) + ('\n' + pad + '}\n').length);
}
var chromeStart = src.indexOf('  var BoardChrome = {');
var chromeEnd = src.indexOf('\n  };', chromeStart) + '\n  };'.length;
if (chromeStart < 0 || chromeEnd < chromeStart) throw new Error('BoardChrome not found');

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function flush() {
  return new Promise(function (resolve) { setImmediate(resolve); });
}
function response(ok, body) {
  return { ok: ok, text: function () { return Promise.resolve(body); } };
}

function element(tag) {
  var listeners = {};
  var attrs = {};
  var el = {
    tag: tag,
    children: [],
    style: { setProperty: function (k, v) { el.style[k] = v; } },
    dataset: {},
    hidden: false,
    disabled: false,
    textContent: '',
    setAttribute: function (k, v) { attrs[k] = String(v); },
    getAttribute: function (k) { return k in attrs ? attrs[k] : null; },
    appendChild: function (child) { el.children.push(child); return child; },
    addEventListener: function (type, fn) { (listeners[type] = listeners[type] || []).push(fn); },
    click: function () { (listeners.click || []).forEach(function (fn) { fn(); }); },
    classList: { add: function () {}, remove: function () {} },
    label: function () {
      return el.textContent + el.children.map(function (c) { return c.text || ''; }).join('');
    },
  };
  return el;
}

var epicHoverTried, hoveredAct, hideCount, trigger0;
var calls, toasts, refreshed, reloads, menusClosed, replies, els, umbrellaLine, tmId, cardMenuCard, epicSubmenuLoading, opened;
var window = {};
var CARD_SUB_ICONS = {};
var BoardChrome;
var TimerUI;

function reset() {
  calls = [];
  toasts = [];
  refreshed = [];
  reloads = 0;
  menusClosed = 0;
  replies = [];
  els = { 'tm-epic-list': element('div'), 'card-ctx-menu': element('div'), 'card-ctx-submenu': element('div') };
  els['card-ctx-submenu'].hidden = true;
  epicHoverTried = false;
  hoveredAct = null;
  hideCount = 0;
  trigger0 = element('button');
  els['tm-epic-list'].hidden = true;
  umbrellaLine = false;
  tmId = 'c1';
  cardMenuCard = null;
  epicSubmenuLoading = false;
  opened = [];
  TimerUI = { refreshTaskCard: function (id) { refreshed.push(id); } };
  window = { TimerUI: TimerUI, location: { reload: function () { reloads++; } } };
}

var document = {
  getElementById: function (id) { return els[id] || null; },
  querySelector: function (sel) {
    if (sel === '[data-tm-act="add-to-epic"]') return trigger0;
    return sel === '#modal-root .tm-umbrella-line' && umbrellaLine ? {} : null;
  },
  querySelectorAll: function () { return []; },
  createElement: element,
  createTextNode: function (text) { return { text: text }; },
};
function fetch(path, init) {
  calls.push({ path: path, method: init.method, headers: init.headers, body: init.body });
  return Promise.resolve(replies.shift());
}
function toast(message) { toasts.push(message); }
function modalTaskId() { return tmId; }
function closeAllTmMenus() { menusClosed++; }
function refreshAfterLinkChange() { refreshed.push('modal'); }
function openCardSubmenu(btn, build) {
  var sub = element('div');
  build(sub);
  opened.push(sub);
}
function boardId() { return 'b1'; }
function hideCardSubmenu() { hideCount++; }

eval(fn('  function linkError(message) {'));
eval(fn('  function linkFailure(err, fallback) {'));
eval(fn('  function epicRequest(path, method, body) {'));
eval(fn('  function epicLinkCall(taskId, epicId, hasParent) {'));
eval(fn('  function addToEpic(taskId, epicId, hasParent, refresh) {'));
eval(fn('  function appendEpicChoices(container, choices, makeButton) {'));
eval(fn('  function addCardToEpic(card, epicId) {'));
eval(fn('  function toggleModalEpicList(trigger) {'));
eval(fn('    function cardSubmenuButton(label, subAct, extra) {', '    '));
eval(fn('    function openEpicSubmenu(btn, task) {', '    '));
eval(fn('    function hoverCardMenuItem(item) {', '    '));
var realCloseAllTmMenus = eval('(' + fn('  function closeAllTmMenus() {').replace('function closeAllTmMenus', 'function') + ')');
BoardChrome = eval('(function () {' + src.slice(chromeStart, chromeEnd) + '\n return BoardChrome; })()');

var JSON_HEADERS = { 'Content-Type': 'application/json', 'Accept': 'application/json' };

function epic(id, name, extra) {
  var base = { id: id, name: name, closed: false, parent_id: null };
  Object.keys(extra || {}).forEach(function (k) { base[k] = extra[k]; });
  return base;
}

function names(choices) {
  return choices && choices.map(function (c) { return c.depth + ':' + c.name; });
}

function card(hasPill) {
  return {
    dataset: { taskId: 'c1' },
    querySelector: function (sel) { return sel === '.card-epic-tag' && hasPill ? {} : null; },
  };
}

function trigger() {
  var t = element('button');
  t.setAttribute('aria-expanded', 'false');
  return t;
}

(async function () {
  reset();
  BoardChrome.epics = [
    epic('e1', 'Alpha'),
    epic('e2', 'Beta', { closed: true }),
    epic('e3', 'Alpha child', { parent_id: 'e1' }),
    epic('e4', 'Gamma'),
    epic('c1', 'The card itself'),
    epic('e5', 'Under the card', { parent_id: 'c1' }),
    epic('e6', 'Below that', { parent_id: 'e5' }),
    epic('e7', 'Under a closed epic', { parent_id: 'e2' }),
  ];
  BoardChrome.epicsLoaded = true;
  var got = await BoardChrome.epicChoices('c1');
  check('lists open epics only, minus the item and its descendants, nested under parents', names(got),
    ['0:Alpha', '1:Alpha child', '0:Gamma', '0:Under a closed epic']);

  reset();
  BoardChrome.epics = [];
  BoardChrome.epicsLoaded = false;
  var loads = 0;
  BoardChrome.loadEpics = function () {
    loads++;
    BoardChrome.epics = [epic('e1', 'Alpha')];
    BoardChrome.epicsLoaded = true;
    return Promise.resolve();
  };
  got = await BoardChrome.epicChoices('c1');
  check('an unloaded list loads once before it is listed', [loads, names(got)], [1, ['0:Alpha']]);
  got = await BoardChrome.epicChoices('c1');
  check('a loaded list is not fetched again', loads, 1);

  BoardChrome.epicsLoaded = false;
  BoardChrome.loadEpics = function () { return Promise.resolve(); };
  check('a list that failed to load gives no choices', await BoardChrome.epicChoices('c1'), null);

  BoardChrome.epics = [epic('e2', 'Closed', { closed: true })];
  BoardChrome.epicsLoaded = true;
  got = await BoardChrome.epicChoices('c1');
  check('only closed epics leave an empty list', got, []);
  var box = element('div');
  appendEpicChoices(box, got, function () { throw new Error('no buttons expected'); });
  check('an empty list shows one disabled No open epics item', box.children.map(function (b) {
    return [b.label(), b.disabled, b.tag];
  }), [['No open epics', true, 'button']]);

  reset();
  replies.push(response(true, '{}'));
  addCardToEpic(card(false), 'e1');
  await flush();
  await flush();
  check('a card without an epic is linked as a child with the JSON Accept header', calls, [
    { path: '/api/tasks/e1/links', method: 'POST', headers: JSON_HEADERS, body: '{"kind":"child","target_id":"c1"}' },
  ]);
  check('success re-fetches the card, once', [refreshed, reloads, toasts], [['c1'], 0, []]);

  reset();
  replies.push(response(true, '{}'));
  addCardToEpic(card(true), 'e9');
  await flush();
  await flush();
  check('a card already in an epic uses the umbrella route', calls, [
    { path: '/api/tasks/c1/umbrella', method: 'PUT', headers: JSON_HEADERS, body: '{"target_id":"e9"}' },
  ]);
  check('a moved card is re-fetched', refreshed, ['c1']);

  reset();
  replies.push(response(false, '{"message":"the link would put the umbrella under its own child"}'));
  addCardToEpic(card(false), 'e1');
  await flush();
  await flush();
  check('a refusal toasts the server message and refreshes nothing', [toasts, refreshed, reloads],
    [['the link would put the umbrella under its own child'], [], 0]);

  reset();
  replies.push(response(false, ''));
  addCardToEpic(card(true), 'e1');
  await flush();
  await flush();
  check('an empty refusal falls back to a fixed message', toasts, ['Could not add the card to the epic.']);

  reset();
  window.TimerUI = undefined;
  TimerUI = undefined;
  replies.push(response(true, '{}'));
  addCardToEpic(card(false), 'e1');
  await flush();
  await flush();
  check('without the card refresher the page reloads', reloads, 1);

  reset();
  BoardChrome.epics = [epic('e1', 'Alpha'), epic('e3', 'Kid', { parent_id: 'e1' }), epic('c1', 'Self')];
  BoardChrome.epicsLoaded = true;
  var t = trigger();
  toggleModalEpicList(t);
  await flush();
  check('the modal list shows open epics, nested, minus the item', els['tm-epic-list'].children.map(function (b) {
    return b.label();
  }), ['Alpha', '↳ Kid']);
  check('the modal list opens', [els['tm-epic-list'].hidden, t.getAttribute('aria-expanded')], [false, 'true']);
  toggleModalEpicList(t);
  check('a second click closes the list', [els['tm-epic-list'].hidden, t.getAttribute('aria-expanded')], [true, 'false']);

  toggleModalEpicList(t);
  await flush();
  replies.push(response(true, '{}'));
  els['tm-epic-list'].children[0].click();
  await flush();
  await flush();
  check('picking in the modal closes the menus, links the child and refreshes the modal', [menusClosed, calls, refreshed], [
    1,
    [{ path: '/api/tasks/e1/links', method: 'POST', headers: JSON_HEADERS, body: '{"kind":"child","target_id":"c1"}' }],
    ['modal'],
  ]);

  reset();
  BoardChrome.epics = [epic('e1', 'Alpha')];
  BoardChrome.epicsLoaded = true;
  umbrellaLine = true;
  toggleModalEpicList(trigger());
  await flush();
  replies.push(response(true, '{}'));
  els['tm-epic-list'].children[0].click();
  await flush();
  await flush();
  check('a modal card with an umbrella line uses the umbrella route', calls.map(function (c) {
    return [c.method, c.path, c.body];
  }), [['PUT', '/api/tasks/c1/umbrella', '{"target_id":"e1"}']]);

  reset();
  BoardChrome.epics = [epic('e1', 'Alpha')];
  BoardChrome.epicsLoaded = true;
  toggleModalEpicList(trigger());
  await flush();
  replies.push(response(false, '{"message":"target is not an epic"}'));
  els['tm-epic-list'].children[0].click();
  await flush();
  await flush();
  check('a refused modal link toasts and does not refresh', [toasts, refreshed], [['target is not an epic'], []]);

  reset();
  tmId = 'e1';
  BoardChrome.epics = [epic('e1', 'Self'), epic('e2', 'Own child', { parent_id: 'e1' }), epic('e3', 'Other')];
  BoardChrome.epicsLoaded = true;
  toggleModalEpicList(trigger());
  await flush();
  check('an epic can pick another epic but not itself or its own descendants', els['tm-epic-list'].children.map(function (b) {
    return b.label();
  }), ['Other']);

  reset();
  BoardChrome.epics = [epic('e1', 'Alpha'), epic('e2', 'Kid', { parent_id: 'e1' })];
  BoardChrome.epicsLoaded = true;
  cardMenuCard = card(false);
  els['card-ctx-menu'].hidden = false;
  openEpicSubmenu({}, { id: 'c1' });
  await flush();
  check('the board submenu lists the epics as data-card-sub=epic buttons', opened.map(function (sub) {
    return [sub.dataset.for, sub.children.map(function (b) {
      return [b.getAttribute('data-card-sub'), b.getAttribute('data-epic-id'), b.label()];
    })];
  }), [['epic', [['epic', 'e1', 'Alpha'], ['epic', 'e2', '↳ Kid']]]]);
  check('the loading guard is released', epicSubmenuLoading, false);

  reset();
  BoardChrome.epics = [epic('e1', 'Alpha')];
  BoardChrome.epicsLoaded = true;
  cardMenuCard = card(false);
  els['card-ctx-menu'].hidden = true;
  openEpicSubmenu({}, { id: 'c1' });
  await flush();
  check('a menu that closed while loading opens no submenu', [opened.length, epicSubmenuLoading], [0, false]);

  reset();
  BoardChrome.epics = [];
  BoardChrome.epicsLoaded = true;
  cardMenuCard = card(false);
  els['card-ctx-menu'].hidden = false;
  openEpicSubmenu({}, { id: 'c1' });
  await flush();
  check('an empty board shows the disabled item in the submenu', opened.map(function (sub) {
    return sub.children.map(function (b) { return [b.label(), b.disabled]; });
  }), [[['No open epics', true]]]);

  function hoverItem(act) {
    var item = {
      getAttribute: function () { return act; },
      click: function () { openEpicSubmenu({}, { id: 'c1' }); },
    };
    hoverCardMenuItem(item);
  }

  reset();
  var release;
  BoardChrome.epicsLoaded = false;
  BoardChrome.epicChoices = function () {
    return new Promise(function (resolve) { release = function () { resolve([{ id: 'e1', name: 'Alpha', depth: 0 }]); }; });
  };
  cardMenuCard = card(false);
  els['card-ctx-menu'].hidden = false;
  hoverItem('add-to-epic');
  hoverItem('copy');
  release();
  await flush();
  check('a submenu whose list arrives after the pointer left opens nothing', [opened.length, hoveredAct], [0, 'copy']);

  reset();
  BoardChrome.epicsLoaded = false;
  cardMenuCard = card(false);
  els['card-ctx-menu'].hidden = false;
  var fetches = 0;
  BoardChrome.epicChoices = function () { fetches++; return Promise.resolve(null); };
  hoverItem('add-to-epic');
  await flush();
  hoverItem('add-to-epic');
  hoverItem('add-to-epic');
  await flush();
  check('a failed load is tried once per menu on hover', fetches, 1);
  openEpicSubmenu({}, { id: 'c1' });
  await flush();
  check('a click still retries', fetches, 2);
  check('each failed load toasts', toasts, ['Could not load epics.', 'Could not load epics.']);
  BoardChrome.epicsLoaded = true;
  BoardChrome.epicChoices = function () { fetches++; return Promise.resolve([]); };
  hoverItem('add-to-epic');
  await flush();
  check('hover opens again once the list is loaded', [fetches, opened.length], [3, 1]);

  reset();
  BoardChrome.epicChoices = function () { return Promise.resolve(null); };
  toggleModalEpicList(trigger());
  await flush();
  check('a modal list that cannot load toasts and stays closed', [toasts, els['tm-epic-list'].hidden], [['Could not load epics.'], true]);

  reset();
  var trig = trigger();
  trig.setAttribute('aria-expanded', 'true');
  trigger0 = trig;
  els['tm-epic-list'].hidden = false;
  realCloseAllTmMenus();
  check('closing the menus collapses the epic list and its trigger', [els['tm-epic-list'].hidden, trig.getAttribute('aria-expanded')], [true, 'false']);

  reset();
  var lone = element('div');
  appendEpicChoices(lone, [{ id: 'e1', name: 'Kid', depth: 2 }, { id: 'e2', name: 'Root', depth: 0 }], function (label) {
    var b = element('button');
    b.textContent = label;
    return b;
  });
  check('depth goes to the --depth property, none for a root', lone.children.map(function (b) { return b.style['--depth']; }), [2, undefined]);

  if (failures) process.exit(1);
  console.log('ok');
})();
