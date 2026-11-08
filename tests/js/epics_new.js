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
    value: '',
    disabled: false,
    appendChild: function (child) { el.children.push(child); return child; },
    setAttribute: function (k, v) { el.attrs[k] = String(v); },
    addEventListener: function (type, fn) { (el.listeners[type] = el.listeners[type] || []).push(fn); },
    focus: function () {},
    click: function () { (el.listeners.click || []).forEach(function (fn) { fn({}); }); },
    key: function (key) {
      var ev = {
        key: key,
        prevented: false,
        stopped: false,
        preventDefault: function () { ev.prevented = true; },
        stopPropagation: function () { ev.stopped = true; },
      };
      (el.listeners.keydown || []).forEach(function (fn) { fn(ev); });
      return ev;
    },
    typeText: function (text) {
      el.value = text;
      (el.listeners.input || []).forEach(function (fn) { fn({}); });
    },
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

function load() {
  var body = element('div');
  var panel = element('aside');
  panel.hidden = false;
  panel.dataset.epicTemplate = '## Problem\n\n## Goal\n\n## Approach\n\n## Out of scope';
  var els = { 'epics-body': body, 'epics-panel': panel };
  var document = {
    getElementById: function (id) { return els[id] || null; },
    createElement: element,
    querySelector: function () { return null; },
  };
  var state = { opened: [], toasts: [], fetches: [] };
  var boardId = function () { return 'b1'; };
  var openModal = function (id) { state.opened.push(id); };
  var toast = function (m, d) { state.toasts.push([m, d]); };
  var toastRefusal = function (res, title) { toast(title, res.body); return Promise.resolve(null); };
  var fetch = function (url, opts) {
    var d = deferred();
    state.fetches.push({ url: url, opts: opts || {}, d: d });
    return d.promise;
  };
  var EPICS_REFRESH_DEBOUNCE_MS = 150;
  var window = { setTimeout: function () { return 0; }, clearTimeout: clearTimeout };
  var chrome = eval('(function () {' + src.slice(start, end) + '\n return BoardChrome; })()');
  chrome.epics = [];
  chrome.renderEpics();
  state.chrome = chrome;
  state.body = body;
  state.posts = function () {
    return state.fetches.filter(function (f) { return f.opts.method === 'POST'; });
  };
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

function flush() {
  return new Promise(function (resolve) { setImmediate(resolve); });
}

function openEditor(page) {
  all(page.body, 'epic-new-btn')[0].click();
  return all(page.body, 'epic-new-input')[0];
}

var TEMPLATE = '## Problem\n\n## Goal\n\n## Approach\n\n## Out of scope';

(async function () {
  var page = load();
  check('the button is the first child', page.body.children[0].children[0].className.indexOf('epic-new-btn') !== -1, true);
  check('the button text', all(page.body, 'epic-new-btn')[0].textContent, '+ New epic');
  check('no input before the click', all(page.body, 'epic-new-input').length, 0);

  var input = openEditor(page);
  check('the click swaps in the input', !!input, true);
  check('create and cancel render', [all(page.body, 'epic-new-create').length, all(page.body, 'epic-new-cancel').length], [1, 1]);
  check('the button is gone while editing', all(page.body, 'epic-new-btn').length, 0);

  page = load();
  input = openEditor(page);
  input.typeText('Retry rules');
  var enter = input.key('Enter');
  check('enter is stopped', [enter.prevented, enter.stopped], [true, true]);
  input.key('Enter');
  var posts = page.posts();
  check('enter posts once', posts.length, 1);
  check('post url', posts[0].url, '/api/tasks');
  check('post body', JSON.parse(posts[0].opts.body), { kind: 'epic', board_id: 'b1', name: 'Retry rules', description: TEMPLATE });
  check('json accept', posts[0].opts.headers.Accept, 'application/json');

  page = load();
  input = openEditor(page);
  input.typeText('   ');
  input.key('Enter');
  all(page.body, 'epic-new-create')[0].click();
  check('a blank name posts nothing', page.posts().length, 0);
  check('the editor stays open on a blank name', all(page.body, 'epic-new-input').length, 1);

  page = load();
  input = openEditor(page);
  input.typeText('Draft');
  var esc = input.key('Escape');
  check('escape is stopped', [esc.prevented, esc.stopped], [true, true]);
  check('escape posts nothing', page.posts().length, 0);
  check('escape restores the button', [all(page.body, 'epic-new-btn').length, all(page.body, 'epic-new-input').length], [1, 0]);
  input = openEditor(page);
  check('the draft is gone after cancel', input.value, '');

  page = load();
  input = openEditor(page);
  input.typeText('Draft');
  all(page.body, 'epic-new-cancel')[0].click();
  check('cancel posts nothing', page.posts().length, 0);
  check('cancel restores the button', all(page.body, 'epic-new-btn').length, 1);

  page = load();
  input = openEditor(page);
  input.typeText('Retry rules');
  all(page.body, 'epic-new-create')[0].click();
  page.posts()[0].d.resolve({ ok: false, body: 'name taken' });
  await flush();
  check('a failed post toasts', page.toasts, [['Could not create the epic.', 'name taken']]);
  check('a failed post keeps the input text', all(page.body, 'epic-new-input')[0].value, 'Retry rules');
  check('a failed post opens nothing', page.opened, []);
  all(page.body, 'epic-new-create')[0].click();
  check('a retry posts again', page.posts().length, 2);

  page = load();
  input = openEditor(page);
  input.typeText('Retry rules');
  all(page.body, 'epic-new-create')[0].click();
  page.posts()[0].d.resolve({ ok: true, json: function () { return Promise.resolve({ id: 'e9' }); } });
  await flush();
  var loads = page.fetches.filter(function (f) { return f.opts.method !== 'POST'; });
  check('success reloads the list', loads.map(function (f) { return f.url; }), ['/api/boards/b1/epics']);
  loads[0].d.resolve({
    ok: true,
    json: function () {
      return Promise.resolve([{ id: 'e9', name: 'Retry rules', closed: false, parent_id: null, children_total: 0, children_done: 0 }]);
    },
  });
  await flush();
  await flush();
  check('success opens the new epic', page.opened, ['e9']);
  check('the new epic is listed', all(page.body, 'epic-name').map(function (n) { return n.textContent; }), ['Retry rules']);
  check('the editor closes on success', [all(page.body, 'epic-new-btn').length, all(page.body, 'epic-new-input').length], [1, 0]);

  page = load();
  input = openEditor(page);
  input.typeText('Keep me');
  page.chrome.loadEpics();
  page.fetches[0].d.resolve({ ok: true, json: function () { return Promise.resolve([]); } });
  await flush();
  await flush();
  check('a background refresh keeps the editor and text', [all(page.body, 'epic-new-input')[0] === input, input.value], [true, 'Keep me']);

  page = load();
  input = openEditor(page);
  input.typeText('Retry rules');
  all(page.body, 'epic-new-create')[0].click();
  check('the editor is disabled while the post is in flight', [
    all(page.body, 'epic-new-input')[0].disabled,
    all(page.body, 'epic-new-create')[0].disabled,
    all(page.body, 'epic-new-cancel')[0].disabled,
  ], [true, true, true]);
  page.posts()[0].d.resolve({ ok: false, body: 'no' });
  await flush();
  check('the editor is enabled again after a failure', [
    all(page.body, 'epic-new-input')[0].disabled,
    all(page.body, 'epic-new-cancel')[0].disabled,
  ], [false, false]);

  page = load();
  input = openEditor(page);
  input.typeText('Retry rules');
  all(page.body, 'epic-new-create')[0].click();
  page.posts()[0].d.resolve({ ok: true, json: function () { return Promise.resolve({ id: 'e9' }); } });
  await flush();
  page.fetches.filter(function (f) { return f.opts.method !== 'POST'; })[0].d.resolve({ ok: false, body: 'down' });
  await flush();
  await flush();
  check('a failed reload after success leaves no editor', [all(page.body, 'epic-new-btn').length, all(page.body, 'epic-new-input').length], [1, 0]);
  check('a failed reload still posted once', page.posts().length, 1);

  page = load();
  input = openEditor(page);
  var letter = input.key('a');
  check('a letter key does not reach the document', [letter.stopped, letter.prevented], [true, false]);

  if (failures) process.exit(1);
})();
