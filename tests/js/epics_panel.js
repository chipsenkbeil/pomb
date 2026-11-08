const { CARD_KIND } = require('../../static/pure.js');
var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var BoardChrome = {');
var end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('BoardChrome not found');

function element(tag) {
  var el = {
    tagName: tag,
    children: [],
    dataset: {},
    style: {},
    attrs: {},
    listeners: {},
    className: '',
    own: '',
    disabled: false,
    appendChild: function (child) { el.children.push(child); return child; },
    setAttribute: function (k, v) { el.attrs[k] = String(v); },
    addEventListener: function (type, fn) { (el.listeners[type] = el.listeners[type] || []).push(fn); },
    click: function () { (el.listeners.click || []).forEach(function (fn) { fn({}); }); },
  };
  Object.defineProperty(el, 'textContent', {
    get: function () {
      return el.own + el.children.map(function (c) { return c.textContent; }).join('');
    },
    set: function (v) { el.own = String(v); el.children = []; },
  });
  return el;
}

function walk(node, fn) {
  fn(node);
  node.children.forEach(function (c) { walk(c, fn); });
}

function all(node, cls) {
  var out = [];
  walk(node, function (n) {
    if ((' ' + n.className + ' ').indexOf(' ' + cls + ' ') !== -1) out.push(n);
  });
  return out;
}

function deferred() {
  var d = {};
  d.promise = new Promise(function (resolve, reject) { d.resolve = resolve; d.reject = reject; });
  return d;
}

function epic(id, name, extra) {
  var e = { id: id, name: name, closed: false, parent_id: null, children_total: 0, children_done: 0 };
  Object.keys(extra || {}).forEach(function (k) { e[k] = extra[k]; });
  return e;
}

function load(role) {
  var body = element('div');
  var panel = element('aside');
  panel.hidden = false;
  var els = { 'epics-body': body, 'epics-panel': panel };
  var document = {
    getElementById: function (id) { return els[id] || null; },
    createElement: element,
    querySelector: function (sel) { return role === 'viewer' && sel === '.board-wrap[data-role="viewer"]' ? {} : null; },
  };
  var state = { opened: [], toasts: [], fetches: [], posts: [] };
  var boardId = function () { return 'b1'; };
  var openModal = function (id) { state.opened.push(id); };
  var toast = function (m, d) { state.toasts.push([m, d]); };
  var toastRefusal = function (res, title) { toast(title, res.body); return Promise.resolve(null); };
  var fetch = function (url) {
    var d = deferred();
    state.fetches.push({ url: url, d: d });
    return d.promise;
  };
  var api = function (path, method) {
    var d = deferred();
    state.posts.push({ path: path, method: method, d: d });
    return d.promise;
  };
  var EPICS_REFRESH_DEBOUNCE_MS = 150;
  var window = { setTimeout: setTimeout, clearTimeout: clearTimeout };
  var chrome = eval('(function () {' + src.slice(start, end) + '\n return BoardChrome; })()');
  state.chrome = chrome;
  state.body = body;
  return state;
}

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) {
    failures++;
    console.log('FAIL ' + name + ': got ' + g + ', want ' + w);
  }
}

function render(list, role) {
  var page = load(role);
  page.chrome.epics = list;
  page.chrome.renderEpics();
  return page;
}

function groupNames(page) {
  return all(page.body, 'epic-group').map(function (g) {
    return all(g, 'epic-name').map(function (n) { return n.textContent; });
  });
}

var page = render([
  epic('p', 'P', { children_total: 4, children_done: 1 }),
  epic('k', 'K', { parent_id: 'p', children_total: 2, children_done: 2 }),
]);
check('a parent and child share one group', groupNames(page), [['P', 'K']]);
check('the child has the inside line', all(page.body, 'epic-inside').map(function (n) { return n.textContent; }), ['inside P']);
check('the parent has a fold arrow', all(page.body, 'epic-fold').map(function (n) { return n.textContent; }), ['▾']);
check('progress text', all(page.body, 'epic-done').map(function (n) { return n.textContent; }), ['1 of 4 done', '2 of 2 done']);
check('no closed toggle', all(page.body, 'epics-closed').length, 0);
check('no empty line', all(page.body, 'epics-empty').length, 0);

page.chrome.epicsFolded.p = true;
page.chrome.renderEpics();
check('folding hides the child and keeps it off the roots', groupNames(page), [['P']]);
check('the folded arrow', all(page.body, 'epic-fold')[0].textContent, '▸');
page.chrome.renderEpics();
check('stays folded on another render', groupNames(page), [['P']]);
all(page.body, 'epic-fold')[0].click();
check('the arrow unfolds', groupNames(page), [['P', 'K']]);

page = render([
  epic('a', 'A'),
  epic('b', 'B', { parent_id: 'a' }),
  epic('c', 'C', { parent_id: 'b' }),
]);
check('three levels share a group', groupNames(page), [['A', 'B', 'C']]);
check('the grandchild is inside its parent', all(page.body, 'epic-inside').map(function (n) { return n.textContent; }), ['inside A', 'inside B']);
page.chrome.epicsFolded.a = true;
page.chrome.renderEpics();
check('folding the top hides the whole subtree', groupNames(page), [['A']]);

page = render([epic('a', 'A', { parent_id: 'b' }), epic('b', 'B', { parent_id: 'a' })]);
check('a cycle renders each epic once', groupNames(page), [['A', 'B']]);
page = render([epic('s', 'S', { parent_id: 's' })]);
check('a self parent renders once', groupNames(page), [['S']]);
check('a self parent has no fold arrow', all(page.body, 'epic-fold').length, 0);

page = render([epic('o', 'O', { parent_id: 'gone' }), epic('x', 'X', { closed: true })]);
check('an epic with a missing parent is a root', groupNames(page), [['O']]);
page = render([epic('o', 'O', { parent_id: 'x' }), epic('x', 'X', { closed: true })]);
check('an epic with a closed parent is a root', groupNames(page), [['O']]);
check('no inside line for that root', all(page.body, 'epic-inside').length, 0);

function toggleLabel(p) {
  var t = all(p.body, 'epics-closed-toggle');
  return t.length ? t[0].textContent : null;
}
page = render([epic('x', 'X', { closed: true })]);
check('singular closed label', toggleLabel(page), '▸ 1 closed epic');
check('closed epics start folded', all(page.body, 'epics-closed-list').length, 0);
check('closed only is not the empty state', all(page.body, 'epics-empty').length, 0);
page = render([epic('x', 'X', { closed: true }), epic('y', 'Y', { closed: true, children_total: 3, children_done: 3 })]);
check('plural closed label', toggleLabel(page), '▸ 2 closed epics');
all(page.body, 'epics-closed-toggle')[0].click();
check('unfolded label', toggleLabel(page), '▾ 2 closed epics');
check('unfolded rows', all(page.body, 'epics-closed-list')[0].children.length, 2);
check('closed progress text', all(page.body, 'epic-done').map(function (n) { return n.textContent; }), ['0 of 0 done', '3 of 3 done']);
page.chrome.renderEpics();
check('the closed state survives a re-render', toggleLabel(page), '▾ 2 closed epics');

function newEpicSections(p) {
  var out = [];
  walk(p.body, function (n) { if (n.attrs['aria-label'] === 'New epic') out.push(n); });
  return out.length;
}
check('a member gets the New epic section', newEpicSections(page), 1);
check('a member gets a Reopen per closed epic', all(page.body, 'epic-reopen').length, 2);
page = render([epic('x', 'X', { closed: true }), epic('y', 'Y', { closed: true })], 'viewer');
all(page.body, 'epics-closed-toggle')[0].click();
check('a viewer gets no New epic section', newEpicSections(page), 0);
check('a viewer still sees the closed epics', all(page.body, 'epics-closed-list')[0].children.length, 2);
check('a viewer gets no Reopen', all(page.body, 'epic-reopen').length, 0);

page = render([]);
check('empty state', all(page.body, 'epics-empty').map(function (n) { return n.textContent; }), ['No epics yet']);

page = render([epic('z', 'Z')]);
var bar = all(page.body, 'tm-progress')[0];
check('zero of zero aria', [bar.attrs['aria-valuemin'], bar.attrs['aria-valuemax'], bar.attrs['aria-valuenow']], ['0', '0', '0']);
check('zero of zero width', bar.children[0].style.width, '0%');
page = render([epic('z', 'Z', { children_total: 4, children_done: 1 })]);
bar = all(page.body, 'tm-progress')[0];
check('aria values', [bar.attrs['aria-valuemax'], bar.attrs['aria-valuenow']], ['4', '1']);
check('bar width', bar.children[0].style.width, '25%');

page = render([epic('i', '<img onerror=x>')]);
check('a name stays text', all(page.body, 'epic-name')[0].textContent, '<img onerror=x>');
check('the funnel carries the epic id', all(page.body, 'epic-funnel')[0].dataset.epicId, 'i');
all(page.body, 'epic-name')[0].click();
check('a name click opens the card', page.opened, ['i']);

function reply(f, body) {
  f.d.resolve({ ok: true, json: function () { return Promise.resolve(body); } });
}

function tick() { return new Promise(function (r) { setTimeout(r, 0); }); }

(async function () {
  page = load();
  var first = page.chrome.loadEpics();
  var second = page.chrome.loadEpics();
  check('both loads fetch the board route', page.fetches.map(function (f) { return f.url; }), ['/api/boards/b1/epics', '/api/boards/b1/epics']);
  reply(page.fetches[1], [epic('n', 'Newer')]);
  await second;
  reply(page.fetches[0], [epic('o', 'Older')]);
  await first;
  check('an older response does not overwrite the newer', page.chrome.epics.map(function (e) { return e.name; }), ['Newer']);
  check('the newer list is rendered', groupNames(page), [['Newer']]);

  var cases = [[200, true, 0], [400, false, 1], [403, false, 1]];
  for (var i = 0; i < cases.length; i++) {
    page = load();
    page.chrome.epics = [epic('x', 'X', { closed: true })];
    page.chrome.epicsClosedOpen = true;
    page.chrome.renderEpics();
    var btn = all(page.body, 'epic-reopen')[0];
    btn.click();
    check('reopen posts ' + cases[i][0], [page.posts[0].path, page.posts[0].method], ['/api/tasks/x/reopen', 'POST']);
    check('the button is disabled while posting', btn.disabled, true);
    page.posts[0].d.resolve({ ok: cases[i][1], status: cases[i][0], body: 'refused', text: function () { return Promise.resolve('{"message":"refused"}'); } });
    await tick();
    await tick();
    if (cases[i][1]) {
      check('reopen 200 reloads the list', page.fetches.length, 1);
      check('reopen 200 does not toast', page.toasts.length, 0);
    } else {
      check('reopen ' + cases[i][0] + ' toasts', page.toasts.length, 1);
      check('reopen ' + cases[i][0] + ' does not reload', page.fetches.length, 0);
      check('reopen ' + cases[i][0] + ' re-enables the button', btn.disabled, false);
    }
  }

  if (failures) process.exit(1);
  console.log('epics panel ok');
})();
