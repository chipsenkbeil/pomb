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

const load = (localStorage, saved, status, shared) => {
  const world = shared || { calls: [], state: { renders: 0, server: saved.slice() }, queue: Promise.resolve() };
  const { calls, state } = world;
  const toasts = [];
  const locks = {
    request: (name, run) => {
      const result = world.queue.then(run);
      world.queue = result.catch(() => {});
      return result;
    },
  };
  const window = { localStorage, navigator: { locks } };
  const fetchJson = () => Promise.resolve({ spent_bookmarks: state.server.slice() });
  const toast = (message) => { toasts.push(message); };
  const fetch = (url, init) => {
    const body = init.body === undefined ? null : JSON.parse(init.body);
    calls.push([url, init.method, body]);
    if (status) {
      return Promise.resolve({ ok: false, status, json: () => Promise.resolve({}) });
    }
    if (init.method === 'PUT') {
      state.server = body.spent_bookmarks;
    } else if (init.method === 'POST') {
      state.server = state.server.filter((b) => b.name !== body.name).concat([body]);
    } else {
      const name = decodeURIComponent(url.slice(url.lastIndexOf('/') + 1));
      state.server = state.server.filter((b) => b.name !== name);
    }
    return Promise.resolve({
      ok: true,
      status: 200,
      json: () => Promise.resolve(init.method === 'PUT' ? { spent_bookmarks: state.server } : state.server),
    });
  };
  const apiJson = eval('(() => {' + src.slice(src.indexOf('  function api(path'), src.indexOf('  // read the response body')) + '\nreturn apiJson; })()');
  const SpentBookmarks = eval('(function () {' + code + '\n return SpentBookmarks; })()');
  SpentBookmarks.list = saved;
  SpentBookmarks.ready = true;
  SpentBookmarks.render = () => { state.renders++; };
  return { spent: SpentBookmarks, calls, toasts, state, world };
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

test('spent_bookmark_migrate_should_run_once_when_two_tabs_load_together', () => {
  const ls = store();
  ls.setItem(KEY, JSON.stringify([entry('Week', 'b2')]));
  const first = load(ls, [entry('Sprint', 'b1')], 0);
  const second = load(ls, [entry('Sprint', 'b1')], 0, first.world);
  first.spent.migrate();
  second.spent.migrate();
  return flush().then(() => {
    const merged = [entry('Sprint', 'b1'), entry('Week', 'b2')];
    check('one PUT in all', first.calls, [['/api/settings', 'PUT', { spent_bookmarks: merged }]]);
    check('the server holds the merged list', first.state.server, merged);
    check('the key is removed', ls.getItem(KEY), null);
    check('the second tab shows the saved list', second.spent.list, merged);
    check('no toast', first.toasts.concat(second.toasts), []);
  });
});

test('spent_bookmark_migrate_should_keep_a_bookmark_another_tab_added', () => {
  const ls = store();
  ls.setItem(KEY, JSON.stringify([entry('Week', 'b2')]));
  const page = load(ls, [entry('Sprint', 'b1')], 0);
  page.state.server = [entry('Sprint', 'b1'), entry('Other', 'b8')];
  page.spent.migrate();
  return flush().then(() => {
    const merged = [entry('Sprint', 'b1'), entry('Other', 'b8'), entry('Week', 'b2')];
    check('the PUT keeps the bookmark the other tab saved',
      page.calls, [['/api/settings', 'PUT', { spent_bookmarks: merged }]]);
    check('the list shows the merged entries', page.spent.list, merged);
    check('the key is removed', ls.getItem(KEY), null);
  });
});

test('spent_bookmark_save_should_post_the_bookmark', () => {
  const page = load(store(), [entry('Sprint', 'b1')], 0);
  page.state.server = [entry('Sprint', 'b1'), entry('Other', 'b8')];
  const ids = { 'spent-board': 'b2', 'spent-period': '', 'spent-entry-type': '', 'spent-color': '', 'spent-group': '' };
  const input = { value: 'Week', focus: () => {} };
  global.qs = (id) => (id === 'spent-bookmark-name' ? input : { value: ids[id] });
  page.spent.save();
  return flush().then(() => {
    check('one POST of just the new bookmark',
      page.calls, [['/api/settings/spent-bookmarks', 'POST', { name: 'Week', filters: ids }]]);
    check('the list shows what the server answered', page.spent.list.map((b) => b.name), ['Sprint', 'Other', 'Week']);
    check('the input is cleared', input.value, '');
    check('the page renders once', page.state.renders, 1);
  });
});

test('spent_bookmark_remove_should_send_the_encoded_name', () => {
  const page = load(store(), [entry('Sprint', 'b1'), entry('A/B c', 'b2')], 0);
  return page.spent.remove('A/B c').then(() => {
    check('one DELETE with the encoded name', page.calls, [['/api/settings/spent-bookmarks/A%2FB%20c', 'DELETE', null]]);
    check('the list shows what the server answered', page.spent.list, [entry('Sprint', 'b1')]);
  });
});

test('spent_bookmark_remove_should_toast_when_the_request_fails', () => {
  const page = load(store(), [entry('Sprint', 'b1')], 500);
  return page.spent.remove('Sprint').then(() => {
    check('the failure toasts', page.toasts, ['Could not save the bookmark.']);
    check('the list is unchanged', page.spent.list, [entry('Sprint', 'b1')]);
  });
});

tests.reduce((prev, [name, body]) => prev.then(() => {
  current = name;
  return body();
}), Promise.resolve()).then(() => {
  if (failures) process.exit(1);
  console.log('spent_bookmarks: all checks passed');
});
