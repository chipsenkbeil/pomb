var src = require('fs').readFileSync(process.argv[2], 'utf8');
const apiJson = eval('(() => {' + src.slice(src.indexOf('  function api(path'), src.indexOf('  // read the response body')) + '\nreturn apiJson; })()');
const { CARD_KIND, CARD_SUBMENU, CARD_ACT } = require('../../static/pure.js');
function fn(start, indent) {
  var pad = indent || '  ';
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n' + pad + '}\n', from) + ('\n' + pad + '}\n').length);
}
var chromeStart = src.indexOf('  var BoardChrome = {');
var chromeEnd = src.indexOf('\n  };', chromeStart) + '\n  };'.length;
if (chromeStart < 0 || chromeEnd < chromeStart) throw new Error('BoardChrome not found');

const { check, finish } = require('./support/check.js');
function flush() {
  return new Promise(function (resolve) { setImmediate(resolve); });
}
function response(ok, body) {
  return { ok: ok, text: function () { return Promise.resolve(body); } };
}
const jsonResponse = (ok, body) => ({ ok, json: () => Promise.resolve(body) });

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
    appendChild: (child) => { child.parentNode = el; el.children.push(child); return child; },
    replaceChild: (next, prev) => {
      el.children[el.children.indexOf(prev)] = next;
      next.parentNode = el;
      prev.parentNode = null;
    },
    focus: () => {},
    value: '',
    clickStops: 0,
    key: (key) => {
      const ev = { key: key, preventDefault: () => {}, stopPropagation: () => {} };
      (listeners.keydown || []).forEach((handler) => handler(ev));
    },
    addEventListener: function (type, fn) { (listeners[type] = listeners[type] || []).push(fn); },
    click: () => {
      const ev = { stopPropagation: () => { el.clickStops++; } };
      (listeners.click || []).forEach((fn) => { fn(ev); });
    },
    classList: { add: function () {}, remove: function () {} },
    label: function () {
      return el.textContent + el.children.map(function (c) { return c.text || ''; }).join('');
    },
  };
  return el;
}

var epicHoverTried, hoveredAct, hideCount, trigger0;
var floating, calls, toasts, refreshed, reloads, menusClosed, replies, els, umbrellaLine, umbrellaNames, tmId, cardMenuCard, epicSubmenuLoading, opened;
let pageBoard, taskModal;
var window = {};
var CARD_SUB_ICONS = {};
var BoardChrome;
const refreshBoardCard = (id) => { refreshed.push(id); };

function reset() {
  floating = 0;
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
  umbrellaNames = [];
  tmId = 'c1';
  cardMenuCard = null;
  epicSubmenuLoading = false;
  opened = [];
  pageBoard = 'b1';
  taskModal = null;
  window = { location: { reload: function () { reloads++; } } };
}

var document = {
  getElementById: function (id) { return els[id] || null; },
  querySelector: function (sel) {
    if (sel === '[data-tm-act="' + CARD_ACT.ADD_TO_EPIC + '"]') return trigger0;
    if (sel === '#modal-root .task-modal') return taskModal;
    return sel === '#modal-root .tm-umbrella-line' && umbrellaLine ? {} : null;
  },
  querySelectorAll: (sel) => (sel === '#modal-root .tm-umbrella-line .tm-umbrella-name' ? umbrellaNames : []),
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
function boardId() { return pageBoard; }
function hideFloatingMenus() { floating++; }
function hideCardSubmenu() { hideCount++; }

eval(fn('  function linkError(message) {'));
eval(fn('  function linkFailure(err, fallback) {'));
const withWip = (send) => send(false);
const withForce = (body) => body;
const WIP_DECLINED = new Error('declined');
eval(fn('  function epicRequest(path, method, body, verb) {'));
eval(fn('  function epicLinkCall(taskId, epicId, hasParent) {'));
eval(fn('  function addToEpic(taskId, epicId, hasParent, refresh) {'));
const arrowConst = (name) => {
  const head = '  const ' + name + ' = ';
  const from = src.indexOf(head);
  if (from < 0) throw new Error(name + ' not found');
  const end = src.indexOf('\n  };\n', from) + '\n  };\n'.length;
  return eval('(' + src.slice(from + head.length, end).trim().replace(/;$/, '') + ')');
};
const fetchBoardEpics = arrowConst('fetchBoardEpics');
const epicChoiceList = arrowConst('epicChoiceList');
eval(fn('  function toastRefusal(res, title) {'));
eval(fn('  function postEpic(name) {'));
eval(fn('  function createEpicThen(name, link) {'));
eval(fn('  function newEpicRow(onNew) {'));
eval(fn('  function appendEpicChoices(container, choices, makeButton, currentId, onNew) {'));
eval(fn('  function addCardToEpic(card, epicId) {'));
eval(fn('  function modalTaskBoard() {'));
eval(fn('  function toggleModalEpicList(trigger) {'));
eval(fn('    function cardSubmenuButton(label, subAct, extra) {', '    '));
eval(fn('    function openEpicSubmenu(btn, task) {', '    '));
eval(fn('    function hoverCardMenuItem(item) {', '    '));
var realCloseAllTmMenus = eval('(' + fn('  function closeAllTmMenus() {').replace('function closeAllTmMenus', 'function') + ')');
BoardChrome = eval('(function () {' + src.slice(chromeStart, chromeEnd) + '\n return BoardChrome; })()');
const realEpicChoices = BoardChrome.epicChoices;
const realLoadEpics = BoardChrome.loadEpics;

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
  BoardChrome.epics = [epic('e1', 'Alpha'), epic('e3', 'Kid', { parent_id: 'e1' }), epic('c1', 'Self')];
  BoardChrome.epicsLoaded = true;
  var t = trigger();
  toggleModalEpicList(t);
  await flush();
  check('the modal list shows open epics, nested, minus the item', els['tm-epic-list'].children.map(function (b) {
    return b.label();
  }), ['Alpha', '↳ Kid', '+ New epic…']);
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
  }), ['Other', '+ New epic…']);

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
  }), [['epic', [['epic', 'e1', 'Alpha'], ['epic', 'e2', '↳ Kid'], [null, null, '+ New epic…']]]]);
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
  }), [[['No open epics', true], ['+ New epic…', false]]]);

  const labels = () => els['tm-epic-list'].children.map((b) => b.label());
  const otherEpics = [epic('o1', 'Other epic'), epic('o2', 'Other kid', { parent_id: 'o1' }), epic('o3', 'Other closed', { closed: true })];

  reset();
  pageBoard = 'b1';
  taskModal = { dataset: { boardId: 'b2', boardName: 'Other board' } };
  BoardChrome.epics = [epic('p1', 'Page epic')];
  BoardChrome.epicsLoaded = true;
  replies.push(jsonResponse(true, otherEpics));
  toggleModalEpicList(trigger());
  await flush();
  await flush();
  check('modal_other_board_list: a task on another board lists that board open epics, nested, not the page board', [
    calls.map((c) => c.path), labels(),
  ], [['/api/boards/b2/epics'], ['Other epic', '↳ Other kid']]);
  check('modal_other_board_keeps_page_epics: that fetch leaves BoardChrome.epics and epicsLoaded as the page left them', [
    BoardChrome.epics.map((e) => e.id), BoardChrome.epicsLoaded,
  ], [['p1'], true]);

  reset();
  pageBoard = null;
  taskModal = { dataset: { boardId: 'b2', boardName: 'Other board' } };
  BoardChrome.epics = [];
  BoardChrome.epicsLoaded = false;
  replies.push(jsonResponse(true, otherEpics));
  toggleModalEpicList(trigger());
  await flush();
  await flush();
  check('modal_no_board_page: with no board on the page, the modal loads the task board and shows no failure toast', [
    calls.map((c) => c.path), toasts, labels(),
  ], [['/api/boards/b2/epics'], [], ['Other epic', '↳ Other kid']]);

  reset();
  BoardChrome.epics = [epic('p1', 'Page epic')];
  BoardChrome.epicsLoaded = true;
  taskModal = { dataset: { boardId: 'b1', boardName: 'Page board' } };
  toggleModalEpicList(trigger());
  await flush();
  check('modal_own_board_no_fetch: a task on the page board lists the loaded epics with no extra fetch', [
    calls.length, labels(),
  ], [0, ['Page epic', '+ New epic…']]);

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
  hoverItem(CARD_ACT.ADD_TO_EPIC);
  hoverItem(CARD_ACT.COPY);
  release();
  await flush();
  check('a submenu whose list arrives after the pointer left opens nothing', [opened.length, hoveredAct], [0, CARD_ACT.COPY]);

  reset();
  BoardChrome.epicsLoaded = false;
  cardMenuCard = card(false);
  els['card-ctx-menu'].hidden = false;
  var fetches = 0;
  BoardChrome.epicChoices = function () { fetches++; return Promise.resolve(null); };
  hoverItem(CARD_ACT.ADD_TO_EPIC);
  await flush();
  hoverItem(CARD_ACT.ADD_TO_EPIC);
  hoverItem(CARD_ACT.ADD_TO_EPIC);
  await flush();
  check('a failed load is tried once per menu on hover', fetches, 1);
  openEpicSubmenu({}, { id: 'c1' });
  await flush();
  check('a click still retries', fetches, 2);
  check('each failed load toasts', toasts, ['Could not load epics.', 'Could not load epics.']);
  BoardChrome.epicsLoaded = true;
  BoardChrome.epicChoices = function () { fetches++; return Promise.resolve([]); };
  hoverItem(CARD_ACT.ADD_TO_EPIC);
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

  BoardChrome.epicChoices = realEpicChoices;
  BoardChrome.loadEpics = realLoadEpics;
  const rowState = (box) => box.children.map((b) => [b.label(), b.disabled, b.getAttribute('aria-checked')]);
  const pillCard = (epicId) => ({
    dataset: { taskId: 'c1' },
    querySelector: (sel) => (sel === '.card-epic-tag' && epicId ? { dataset: { epicId: epicId } } : null),
  });
  const nameSpan = (id) => {
    const span = element('span');
    span.setAttribute('data-open-task', id);
    return span;
  };

  reset();
  BoardChrome.epics = [epic('e1', 'Alpha'), epic('e4', 'Gamma')];
  BoardChrome.epicsLoaded = true;
  cardMenuCard = pillCard('e1');
  els['card-ctx-menu'].hidden = false;
  openEpicSubmenu(trigger(), { id: 'c1' });
  await flush();
  await flush();
  check('the board submenu checks and disables the current epic only', rowState(opened[0]), [
    ['Alpha', true, 'true'],
    ['Gamma', false, null],
    ['+ New epic…', false, null],
  ]);

  reset();
  BoardChrome.epics = [epic('e1', 'Alpha'), epic('e4', 'Gamma')];
  BoardChrome.epicsLoaded = true;
  cardMenuCard = pillCard(null);
  els['card-ctx-menu'].hidden = false;
  openEpicSubmenu(trigger(), { id: 'c1' });
  await flush();
  await flush();
  check('a card in no epic checks no board row', rowState(opened[0]), [
    ['Alpha', false, null],
    ['Gamma', false, null],
    ['+ New epic…', false, null],
  ]);

  reset();
  BoardChrome.epics = [epic('e1', 'Alpha'), epic('e4', 'Gamma')];
  BoardChrome.epicsLoaded = true;
  umbrellaNames = [nameSpan('e9'), nameSpan('e1')];
  const modalTrigger = trigger();
  toggleModalEpicList(modalTrigger);
  await flush();
  check('the modal list checks and disables the direct parent only', rowState(els['tm-epic-list']), [
    ['Alpha', true, 'true'],
    ['Gamma', false, null],
    ['+ New epic…', false, null],
  ]);
  replies.push(response(true, '{}'));
  els['tm-epic-list'].children[1].click();
  await flush();
  await flush();
  check('an unchecked modal row still adds the card', calls.map((c) => [c.method, c.path, c.body]), [
    ['POST', '/api/tasks/e4/links', '{"kind":"child","target_id":"c1"}'],
  ]);

  reset();
  BoardChrome.epics = [epic('e1', 'Alpha'), epic('e4', 'Gamma')];
  BoardChrome.epicsLoaded = true;
  umbrellaNames = [nameSpan('e1'), nameSpan('e9')];
  toggleModalEpicList(trigger());
  await flush();
  check('an ancestor above the direct parent checks no modal row', rowState(els['tm-epic-list']), [
    ['Alpha', false, null],
    ['Gamma', false, null],
    ['+ New epic…', false, null],
  ]);

  let reloadedEpics = 0;
  BoardChrome.loadEpics = () => { reloadedEpics++; return Promise.resolve(); };
  const lastOf = (box) => box.children[box.children.length - 1];
  const typeName = (box, text) => {
    lastOf(box).click();
    const input = lastOf(box);
    input.value = text;
    return input;
  };
  const created = jsonResponse(true, { id: 'n1' });
  const postBodies = () => calls.map((c) => [c.method, c.path, c.body]);

  reset();
  reloadedEpics = 0;
  BoardChrome.epics = [epic('e1', 'Alpha')];
  BoardChrome.epicsLoaded = true;
  cardMenuCard = card(false);
  els['card-ctx-menu'].hidden = false;
  openEpicSubmenu({}, { id: 'c1' });
  await flush();
  let input = typeName(opened[0], '  Fresh  ');
  replies.push(created, response(true, '{}'));
  input.key('Enter');
  await flush();
  await flush();
  await flush();
  check('the board row creates the epic then links the card to it and refreshes the card', [postBodies(), refreshed, reloadedEpics, hideCount, floating], [
    [['POST', '/api/tasks', '{"kind":"epic","board_id":"b1","name":"Fresh"}'],
      ['POST', '/api/tasks/n1/links', '{"kind":"child","target_id":"c1"}'],
    ],
    ['c1'], 1, 1, 1,
  ]);

  reset();
  BoardChrome.epics = [];
  BoardChrome.epicsLoaded = true;
  cardMenuCard = pillCard('e9');
  els['card-ctx-menu'].hidden = false;
  openEpicSubmenu({}, { id: 'c1' });
  await flush();
  input = typeName(opened[0], 'First');
  replies.push(created, response(true, '{}'));
  input.key('Enter');
  await flush();
  await flush();
  await flush();
  check('a card already in an epic moves to the new one by the umbrella route', postBodies().map((c) => c[1]), ['/api/tasks', '/api/tasks/c1/umbrella']);

  reset();
  BoardChrome.epics = [];
  BoardChrome.epicsLoaded = true;
  cardMenuCard = card(false);
  els['card-ctx-menu'].hidden = false;
  openEpicSubmenu({}, { id: 'c1' });
  await flush();
  const row = lastOf(opened[0]);
  input = typeName(opened[0], '   ');
  check('opening the row stops the click so the click-away handler leaves the menu open', row.clickStops, 1);
  input.key('Enter');
  await flush();
  check('an empty name posts nothing, links nothing and toasts', [calls, toasts, refreshed], [[], ['Name the epic first.'], []]);
  input.key('Escape');
  check('escape puts the row back', [lastOf(opened[0]).tag, lastOf(opened[0]).label()], ['button', '+ New epic…']);

  reset();
  BoardChrome.epics = [];
  BoardChrome.epicsLoaded = true;
  cardMenuCard = card(false);
  els['card-ctx-menu'].hidden = false;
  openEpicSubmenu({}, { id: 'c1' });
  await flush();
  input = typeName(opened[0], 'Taken');
  replies.push(response(false, '{"message":"name taken"}'));
  input.key('Enter');
  await flush();
  await flush();
  await flush();
  check('a refused create adds no link and shows the refusal toast', [postBodies().map((c) => c[1]), toasts, refreshed], [['/api/tasks'], ['Could not create the epic.'], []]);

  reset();
  reloadedEpics = 0;
  BoardChrome.epics = [epic('e1', 'Alpha')];
  BoardChrome.epicsLoaded = true;
  toggleModalEpicList(trigger());
  await flush();
  input = typeName(els['tm-epic-list'], 'Modal epic');
  replies.push(created, response(true, '{}'));
  input.key('Enter');
  await flush();
  await flush();
  await flush();
  check('the modal row closes the menus, creates the epic, links the card and refreshes the modal', [menusClosed, postBodies(), refreshed, reloadedEpics], [
    1,
    [['POST', '/api/tasks', '{"kind":"epic","board_id":"b1","name":"Modal epic"}'],
      ['POST', '/api/tasks/n1/links', '{"kind":"child","target_id":"c1"}'],
    ],
    ['modal'], 1,
  ]);

  reset();
  BoardChrome.epics = [];
  BoardChrome.epicsLoaded = true;
  toggleModalEpicList(trigger());
  await flush();
  check('the modal list of an empty board still ends with the new epic row', els['tm-epic-list'].children.map((b) => [b.label(), b.disabled]), [
    ['No open epics', true], ['+ New epic…', false],
  ]);
  input = typeName(els['tm-epic-list'], '');
  input.key('Enter');
  await flush();
  check('an empty modal name adds no link and toasts', [calls, toasts, menusClosed], [[], ['Name the epic first.'], 0]);

  reset();
  BoardChrome.epics = [];
  BoardChrome.epicsLoaded = true;
  toggleModalEpicList(trigger());
  await flush();
  input = typeName(els['tm-epic-list'], 'Refused');
  replies.push(response(false, '{"message":"no"}'));
  input.key('Enter');
  await flush();
  await flush();
  await flush();
  check('a refused modal create adds no link and toasts', [postBodies().map((c) => c[1]), toasts, refreshed], [['/api/tasks'], ['Could not create the epic.'], []]);

  finish();
})();
