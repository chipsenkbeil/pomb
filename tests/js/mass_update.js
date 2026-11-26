const { check, finish } = require('./support/check.js');
const { isoDate, previewCell } = require('../../static/pure.js');
const prelude = require('./support/report_prelude.js');
const src = require('fs').readFileSync(process.argv[2], 'utf8');
const { fn } = prelude.extractors(src);
const accountPrefs = prelude.accountPrefs(1);
eval(fn('  function daysIntoWeek(d) {'));
eval(fn('  function isoDay(d) {'));
eval(fn('  function parseDueAt(s) {'));
eval(fn('  function periodToRange(preset'));
const dateMatchesFrom = src.indexOf('    dateMatches: function (value, dueAt, dueDone) {');
const dateMatchesEnd = src.indexOf('\n    },\n', dateMatchesFrom) + '\n    },\n'.length;
if (dateMatchesFrom < 0 || dateMatchesEnd < dateMatchesFrom) throw new Error('dateMatches not found');
const BoardChrome = eval('({' + src.slice(dateMatchesFrom, dateMatchesEnd) + '})');
const withForceFrom = src.indexOf('  const withForce = ') + '  const withForce = '.length;
const withForce = eval(src.slice(withForceFrom, src.indexOf(';\n', withForceFrom)));
const start = src.indexOf('  var MassUpdate = (function () {');
const end = src.indexOf('\n  })();', start) + '\n  })();'.length;
if (start < 0 || end < start) throw new Error('MassUpdate not found');

const dayOffset = (days) => {
  const d = new Date();
  d.setDate(d.getDate() + days);
  return d;
};
const dayStr = (days) => isoDate(dayOffset(days));
const dueOn = (days) => dayStr(days) + 'T12:00:00';
const STEP_NAMES = ['filter', 'select', 'action', 'preview'];

const makeEl = (tag) => {
  const el = {
    tag, value: '', hidden: false, disabled: false, checked: false, selected: false,
    textContent: '', className: '', options: [], children: [], dataset: {}, handlers: {},
    lastChild: null,
    appendChild(child) {
      if (child.tag === 'option') {
        el.options.push(child);
        if (el.options.length === 1) el.value = child.value;
      }
      el.children.push(child);
      el.lastChild = child;
    },
    addEventListener(name, fn) { el.handlers[name] = fn; },
    setAttribute() {},
    focus() {},
  };
  Object.defineProperty(el, 'innerHTML', {
    get() { return ''; },
    set() { el.options = []; el.children = []; el.lastChild = null; },
  });
  return el;
};

const makeCard = (id, columnId, extra) => {
  const list = { dataset: { columnId } };
  const dataset = Object.assign({ taskId: id, taskName: 'Card ' + id, colorValue: 'red', labels: '[]' }, extra);
  return { dataset, closest: () => list };
};

const fixtureCards = () => [
  makeCard('a', 'c1', { labels: '["bug"]', memberIds: 'u1', dueAt: dueOn(-10), dueDone: '0' }),
  makeCard('b', 'c1', { colorValue: 'blue', labels: '["ui"]' }),
  makeCard('c', 'c2', { memberIds: 'u2', groupDay: dayStr(0) }),
  makeCard('d', 'c2', { colorValue: 'green', groupDay: dayStr(-40) }),
  makeCard('e', 'c3', { colorValue: 'blue', labels: '["bug","ui"]', memberIds: 'u1,u2', dueAt: dueOn(30), dueDone: '0' }),
];

const load = () => {
  const els = {};
  const sections = {};
  STEP_NAMES.forEach((name) => { sections[name] = makeEl('section'); });
  const cards = fixtureCards();
  const headers = [
    { dataset: { columnId: 'c1', columnName: 'Todo', isDone: '0' } },
    { dataset: { columnId: 'c2', columnName: 'Done', isDone: '1' } },
    { dataset: { columnId: 'c3', columnName: 'Doing', isDone: '0' } },
  ];
  const swatches = [{ dataset: { value: 'red', label: 'Red', id: 'k1' } }, { dataset: { value: 'blue', label: 'Blue', id: 'k2' } }];
  const document = {
    getElementById: (id) => { if (!els[id]) els[id] = makeEl('div'); return els[id]; },
    createElement: (tag) => makeEl(tag),
    querySelector: (sel) => {
      const m = /data-mu-step="(\w+)"/.exec(sel);
      return m ? sections[m[1]] : null;
    },
    querySelectorAll: (sel) => {
      if (sel === '.task-card') return cards;
      if (sel === '#column-headers-row .columnHeader') return headers;
      if (sel.indexOf('#board-colors') === 0) return swatches;
      return [];
    },
  };
  const calls = [];
  const pending = [];
  const api = (path, method, body) => {
    calls.push({ path, method, body });
    return new Promise((resolve) => { pending.push(resolve); });
  };
  const fetchJson = () => Promise.resolve([]);
  const withWip = (send) => send(false);
  const boardId = () => 'b1';
  const boardNameFor = () => 'Board';
  const showConfirmDialog = () => {};
  const window = { location: { reload() {} } };
  eval(fn('  function cssEscape(s) {'));
  const mu = eval('(function () {' + src.slice(start, end) + '\n return MassUpdate; })()');
  const byId = (id) => document.getElementById(id);
  const click = (id) => byId(id).handlers.click();
  const rows = () => byId('mu-list').children.map((row) => row.children[0]);
  const rowIds = () => rows().map((box) => box.dataset.taskId);
  const flush = () => new Promise((resolve) => { setImmediate(resolve); });
  const respondWith = (index, res) => pending[index](res);
  const respond = (index, tasks) => respondWith(index, {
    ok: true,
    text: () => Promise.resolve(JSON.stringify({ tasks })),
  });
  mu.open();
  return { byId, click, rows, rowIds, flush, respond, respondWith, calls, sections };
};

const filtered = (setup) => {
  const page = load();
  setup(page);
  page.click('mu-next');
  return page.rowIds();
};

let current = '';
const verify = (name, got, want) => check(current + ' ' + name, got, want);

const tests = {
  mass_update_should_list_every_card_with_no_filter: () => {
    verify('all', filtered(() => {}), ['a', 'b', 'c', 'd', 'e']);
  },

  mass_update_should_narrow_by_columns_and_by_period_in_done_columns: () => {
    verify('columns', filtered((p) => { p.byId('mu-columns').options[0].selected = true; }), ['a', 'b']);
    verify('two columns', filtered((p) => {
      p.byId('mu-columns').options[0].selected = true;
      p.byId('mu-columns').options[2].selected = true;
    }), ['a', 'b', 'e']);
    verify('period only limits done columns', filtered((p) => { p.byId('mu-period').value = 'this-month'; }), ['a', 'b', 'c', 'e']);
    verify('columns and period', filtered((p) => {
      p.byId('mu-columns').options[1].selected = true;
      p.byId('mu-period').value = 'this-month';
    }), ['c']);
  },

  mass_update_should_narrow_by_color: () => {
    verify('match', filtered((p) => { p.byId('mu-color').value = 'blue'; }), ['b', 'e']);
    verify('non-match', filtered((p) => { p.byId('mu-color').value = 'purple'; }), []);
  },

  mass_update_should_narrow_by_label: () => {
    verify('match', filtered((p) => { p.byId('mu-label').value = 'bug'; }), ['a', 'e']);
    verify('non-match', filtered((p) => { p.byId('mu-label').value = 'missing'; }), []);
  },

  mass_update_should_list_the_labels_cards_carry_sorted_once: () => {
    const page = load();
    verify('options', page.byId('mu-label').options.map((o) => o.value), ['all', 'bug', 'ui']);
  },

  mass_update_should_narrow_by_member: () => {
    verify('match', filtered((p) => { p.byId('mu-member').value = 'u2'; }), ['c', 'e']);
    verify('non-match', filtered((p) => { p.byId('mu-member').value = 'u9'; }), []);
  },

  mass_update_should_narrow_to_unassigned_cards: () => {
    verify('unassigned', filtered((p) => { p.byId('mu-member').value = 'none'; }), ['b', 'd']);
    const page = load();
    verify('option', page.byId('mu-member').options.map((o) => o.textContent), ['Any', 'Unassigned']);
  },

  mass_update_should_narrow_by_due_state: () => {
    verify('has match', filtered((p) => { p.byId('mu-due').value = 'has'; }), ['a', 'e']);
    verify('none match', filtered((p) => { p.byId('mu-due').value = 'none'; }), ['b', 'c', 'd']);
    verify('overdue match', filtered((p) => { p.byId('mu-due').value = 'overdue'; }), ['a']);
  },

  mass_update_should_narrow_by_due_before_and_after_a_day: () => {
    verify('before match', filtered((p) => {
      p.byId('mu-due').value = 'before';
      p.byId('mu-due-day').value = dayStr(20);
    }), ['a']);
    verify('before non-match', filtered((p) => {
      p.byId('mu-due').value = 'before';
      p.byId('mu-due-day').value = dayStr(-20);
    }), []);
    verify('after match', filtered((p) => {
      p.byId('mu-due').value = 'after';
      p.byId('mu-due-day').value = dayStr(20);
    }), ['e']);
    verify('after non-match', filtered((p) => {
      p.byId('mu-due').value = 'after';
      p.byId('mu-due-day').value = dayStr(60);
    }), []);
    verify('no day matches nothing', filtered((p) => { p.byId('mu-due').value = 'before'; }), []);
  },

  mass_update_should_show_the_due_day_field_only_for_before_and_after: () => {
    const page = load();
    const seen = ['all', 'before', 'after', 'has'].map((value) => {
      page.byId('mu-due').value = value;
      page.byId('mu-due').handlers.change();
      return page.byId('mu-due-day-field').hidden;
    });
    verify('hidden', seen, [true, false, false, true]);
  },

  mass_update_should_combine_filters: () => {
    verify('color and label', filtered((p) => {
      p.byId('mu-color').value = 'blue';
      p.byId('mu-label').value = 'bug';
    }), ['e']);
  },

  mass_update_should_pick_every_match_and_disable_next_when_none_picked: () => {
    const page = load();
    page.click('mu-next');
    verify('picked', page.rows().map((box) => box.checked), [true, true, true, true, true]);
    verify('count', page.byId('mu-count').textContent, '5 tasks match your filter');
    page.click('mu-clear-all');
    verify('next disabled', page.byId('mu-next').disabled, true);
    page.click('mu-select-all');
    verify('next enabled', page.byId('mu-next').disabled, false);
  },

  mass_update_should_keep_the_choices_when_previous_is_pressed: () => {
    const page = load();
    page.byId('mu-color').value = 'blue';
    page.click('mu-next');
    const box = page.rows()[0];
    box.checked = false;
    page.byId('mu-list').handlers.change({ target: { closest: () => box } });
    page.click('mu-next');
    verify('on action', page.sections.action.hidden, false);
    page.click('mu-prev');
    verify('back on select', page.sections.select.hidden, false);
    verify('picked kept', page.rows().map((row) => [row.dataset.taskId, row.checked]), [['b', false], ['e', true]]);
    page.click('mu-prev');
    verify('back on filter', page.sections.filter.hidden, false);
    verify('prev hidden on first step', page.byId('mu-prev').hidden, true);
    verify('filter kept', page.byId('mu-color').value, 'blue');
    page.click('mu-next');
    verify('picked survive a round trip', page.rows().map((row) => [row.dataset.taskId, row.checked]), [['b', false], ['e', true]]);
  },

  mass_update_should_reset_the_picks_when_the_filter_changes: () => {
    const page = load();
    page.byId('mu-color').value = 'blue';
    page.click('mu-next');
    page.rows()[0].checked = false;
    page.byId('mu-list').handlers.change({ target: { closest: () => page.rows()[0] } });
    page.click('mu-prev');
    page.byId('mu-color').value = 'red';
    page.click('mu-next');
    verify('rows', page.rows().map((row) => [row.dataset.taskId, row.checked]), [['a', true], ['c', true]]);
  },

  mass_update_should_send_a_dry_run_and_render_the_preview: async () => {
    const page = load();
    page.byId('mu-color').value = 'blue';
    page.click('mu-next');
    page.click('mu-next');
    page.byId('mu-move-column').value = 'c2';
    page.click('mu-next');
    verify('apply disabled while waiting', page.byId('mu-apply').disabled, true);
    verify('request', page.calls.map((c) => [c.path, c.method, c.body.dry_run, c.body.action, c.body.task_ids, c.body.column_id]),
      [['/api/boards/b1/tasks/bulk', 'POST', true, 'move', ['b', 'e'], 'c2']]);
    page.respond(0, [{ name: 'Card b', before: 'Todo', after: 'Done' }, { name: 'Card e', before: null, after: 'Done' }]);
    await page.flush();
    verify('count', page.byId('mu-preview-count').textContent, '2 tasks will be updated');
    verify('rows', page.byId('mu-preview-body').children.map((row) => row.children.map((cell) => cell.textContent)),
      [['Card b', 'Todo', 'Done'], ['Card e', previewCell(null), 'Done']]);
    verify('apply enabled', page.byId('mu-apply').disabled, false);
  },

  mass_update_should_drop_a_dry_run_answer_after_the_user_left_the_preview: async () => {
    const page = load();
    page.click('mu-next');
    page.click('mu-next');
    page.byId('mu-move-column').value = 'c2';
    page.click('mu-next');
    page.click('mu-prev');
    page.respond(0, [{ name: 'Card a', before: 'Todo', after: 'Done' }]);
    await page.flush();
    verify('on action', page.sections.action.hidden, false);
    verify('no rows', page.byId('mu-preview-body').children.length, 0);
    verify('count untouched', page.byId('mu-preview-count').textContent, '0 tasks will be updated');
    verify('apply still disabled', page.byId('mu-apply').disabled, true);
  },

  mass_update_should_drop_an_older_dry_run_answer_after_a_newer_request: async () => {
    const page = load();
    page.click('mu-next');
    page.click('mu-next');
    page.byId('mu-move-column').value = 'c2';
    page.click('mu-next');
    page.click('mu-prev');
    page.click('mu-next');
    verify('two requests', page.calls.length, 2);
    page.respond(1, [{ name: 'Card new', before: 'Todo', after: 'Done' }]);
    await page.flush();
    page.respond(0, [{ name: 'Card old', before: 'Todo', after: 'Done' }, { name: 'Card old 2', before: 'Todo', after: 'Done' }]);
    await page.flush();
    verify('newest answer wins', page.byId('mu-preview-body').children.map((row) => row.children[0].textContent), ['Card new']);
    verify('count', page.byId('mu-preview-count').textContent, '1 task will be updated');
  },

  mass_update_should_return_to_the_action_step_with_the_refusal_message: async () => {
    const page = load();
    page.click('mu-next');
    page.click('mu-next');
    page.byId('mu-move-column').value = 'c2';
    page.click('mu-next');
    page.respondWith(0, { ok: false, text: () => Promise.resolve(JSON.stringify({ message: 'Nope' })) });
    await page.flush();
    verify('back on action', page.sections.action.hidden, false);
    verify('error shown', page.byId('mu-error').textContent, 'Nope');
    verify('apply disabled', page.byId('mu-apply').disabled, true);
  },

  mass_update_should_refuse_the_preview_without_a_target_column: () => {
    const page = load();
    page.click('mu-next');
    page.click('mu-next');
    page.byId('mu-move-column').value = '';
    page.click('mu-next');
    verify('back on action', page.sections.action.hidden, false);
    verify('error', page.byId('mu-error').textContent, 'Choose a column to move the tasks to.');
    verify('nothing sent', page.calls.length, 0);
  },
};

const run = async () => {
  for (const name of Object.keys(tests)) {
    try {
      current = name;
      await tests[name]();
    } catch (e) {
      check(name + ' threw', e.stack, null);
    }
  }
  finish();
};

run();
