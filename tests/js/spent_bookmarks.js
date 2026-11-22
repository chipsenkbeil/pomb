const src = require('fs').readFileSync(process.argv[2], 'utf8');
const from = src.indexOf('    const MAX_SPENT_BOOKMARKS = 50;');
const objectAt = src.indexOf('    var SpentBookmarks = {', from);
const to = src.indexOf('\n    };', objectAt) + '\n    };'.length;
if (from < 0 || objectAt < 0 || to < objectAt) throw new Error('SpentBookmarks not found');
const code = src.slice(from, to);

const KEY = 'pomb-spent-bookmarks';
const LIMIT_TOAST = 'Some old bookmarks were over the limit and were dropped.';
const FAIL_TOAST = 'Could not move the old bookmarks to your account.';

const store = (initial) => {
  const m = Object.assign({}, initial || {});
  return {
    getItem: (k) => (k in m ? m[k] : null),
    setItem: (k, v) => { m[k] = String(v); },
    removeItem: (k) => { delete m[k]; },
  };
};

const entry = (name, board) => ({ name, filters: { 'spent-board': board } });

const load = (localStorage, saved, status, locks) => {
  const calls = [];
  const toasts = [];
  const state = { renders: 0, server: saved.slice(), events: [] };
  const window = { localStorage };
  if (locks) window.navigator = { locks };
  const toast = (message) => { toasts.push(message); };
  const fetchJson = (url) => {
    state.events.push('get');
    return Promise.resolve({ spent_bookmarks: state.server });
  };
  const fetch = (url, init) => {
    state.events.push('put');
    calls.push([url, init.method, JSON.parse(init.body)]);
    if (status) {
      return Promise.resolve({ ok: false, status, json: () => Promise.resolve({}) });
    }
    state.server = JSON.parse(init.body).spent_bookmarks;
    return Promise.resolve({
      ok: true,
      status: 200,
      json: () => Promise.resolve({ spent_bookmarks: state.server }),
    });
  };
  const SpentBookmarks = eval('(function () {' + code + '\n return SpentBookmarks; })()');
  SpentBookmarks.list = saved;
  SpentBookmarks.ready = true;
  SpentBookmarks.render = () => { state.renders++; };
  return { spent: SpentBookmarks, calls, toasts, state };
};

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

let failures = 0;
let current = '';
const check = (name, got, want) => {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + current + ' ' + name + ': got ' + g + ' want ' + w); }
};

const tests = [];
const test = (name, body) => { tests.push([name, body]); };

test('spent_bookmark_migrate_should_save_old_entries_and_remove_the_key', () => {
  const ls = store();
  ls.setItem(KEY, JSON.stringify([entry('Week', 'b2'), entry('Quarter', 'b3')]));
  const page = load(ls, [entry('Sprint', 'b1')], 0);
  page.spent.migrate();
  return flush().then(() => {
    const merged = [entry('Sprint', 'b1'), entry('Week', 'b2'), entry('Quarter', 'b3')];
    check('one PUT of the merged list', page.calls, [['/api/settings', 'PUT', { spent_bookmarks: merged }]]);
    check('the list shows the saved entries', page.spent.list, merged);
    check('the key is removed after the save', ls.getItem(KEY), null);
    check('the page renders once', page.state.renders, 1);
    check('no toast on success', page.toasts, []);
  });
});

test('spent_bookmark_migrate_should_not_add_a_name_twice', () => {
  const ls = store();
  ls.setItem(KEY, JSON.stringify([entry('Week', 'b4'), entry('New', 'b5'), entry('New', 'b6')]));
  const page = load(ls, [entry('Week', 'b9')], 0);
  page.spent.migrate();
  return flush().then(() => {
    check('the saved Week wins and New is added once',
      page.calls, [['/api/settings', 'PUT', { spent_bookmarks: [entry('Week', 'b9'), entry('New', 'b5')] }]]);
    check('the key is removed after the save', ls.getItem(KEY), null);

    const allSaved = store();
    allSaved.setItem(KEY, JSON.stringify([entry('Week', 'b4')]));
    const quiet = load(allSaved, [entry('Week', 'b9')], 0);
    quiet.spent.migrate();
    return flush().then(() => {
      check('nothing new means no PUT', quiet.calls, []);
      check('nothing new still removes the key', allSaved.getItem(KEY), null);
    });
  });
});

test('spent_bookmark_migrate_should_keep_the_key_when_the_save_fails', () => {
  const ls = store();
  const old = JSON.stringify([entry('Week', 'b2')]);
  ls.setItem(KEY, old);
  const page = load(ls, [], 500);
  page.spent.migrate();
  return flush().then(() => {
    check('the save was attempted once', page.calls.length, 1);
    check('the failure toasts', page.toasts, [FAIL_TOAST]);
    check('the key is kept', ls.getItem(KEY), old);
    check('the list is unchanged', page.spent.list, []);
    check('the page does not render', page.state.renders, 0);
  });
});

test('spent_bookmark_migrate_should_drop_entries_past_the_limit', () => {
  const ls = store();
  ls.setItem(KEY, JSON.stringify([entry('Overflow', 'b7')]));
  const saved = [];
  for (let i = 0; i < 50; i++) saved.push(entry('S' + i, 'b1'));
  const page = load(ls, saved, 0);
  page.spent.migrate();
  return flush().then(() => {
    check('the PUT keeps the 50 saved entries and drops the new one',
      page.calls, [['/api/settings', 'PUT', { spent_bookmarks: saved }]]);
    check('the limit toast shows', page.toasts, [LIMIT_TOAST]);
    check('the key is removed after the save', ls.getItem(KEY), null);
  });
});

test('spent_bookmark_save_should_keep_a_bookmark_another_tab_saved', () => {
  const page = load(store(), [entry('Sprint', 'b1')], 0);
  page.state.server = [entry('Sprint', 'b1'), entry('Other', 'b8')];
  const ids = { 'spent-board': 'b2', 'spent-period': '', 'spent-entry-type': '', 'spent-color': '', 'spent-group': '' };
  const input = { value: 'Week', focus: () => {} };
  global.qs = (id) => (id === 'spent-bookmark-name' ? input : { value: ids[id] });
  page.spent.save();
  return flush().then(() => {
    check('the PUT keeps the other tab bookmark and adds the new one',
      page.calls[0][2].spent_bookmarks.map((b) => b.name), ['Sprint', 'Other', 'Week']);
    check('the list shows the saved result', page.spent.list.map((b) => b.name), ['Sprint', 'Other', 'Week']);
    check('the input is cleared', input.value, '');
  });
});

test('spent_bookmark_remove_should_keep_a_bookmark_another_tab_saved', () => {
  const page = load(store(), [entry('Sprint', 'b1'), entry('Week', 'b2')], 0);
  page.state.server = [entry('Sprint', 'b1'), entry('Week', 'b2'), entry('Other', 'b8')];
  return page.spent.remove('Week').then(() => {
    check('the PUT drops only the named bookmark',
      page.calls, [['/api/settings', 'PUT', { spent_bookmarks: [entry('Sprint', 'b1'), entry('Other', 'b8')] }]]);
    check('the list shows the saved result', page.spent.list, [entry('Sprint', 'b1'), entry('Other', 'b8')]);
  });
});

test('spent_bookmark_write_should_take_the_web_lock_when_present', () => {
  const requested = [];
  const locks = {
    request: (name, run) => {
      requested.push(name);
      page.state.events.push('lock');
      return run();
    },
  };
  const page = load(store(), [entry('Sprint', 'b1')], 0, locks);
  return page.spent.remove('Sprint').then(() => {
    check('the lock name', requested, ['pomb-spent-bookmarks']);
    check('the read and PUT run inside the lock', page.state.events, ['lock', 'get', 'put']);
    check('the list is saved', page.spent.list, []);
  });
});

tests.reduce((prev, [name, body]) => prev.then(() => {
  current = name;
  return body();
}), Promise.resolve()).then(() => {
  if (failures) process.exit(1);
  console.log('spent_bookmarks: all checks passed');
});
