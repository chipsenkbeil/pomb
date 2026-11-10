const { boardEventPlan } = require('../../static/pure.js');

let failures = 0;
function check(name, got, want) {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

const cards = { t1: 'c1', t2: 'c2' };

check('plan_should_refresh_an_updated_card',
  boardEventPlan([{ kind: 'task_updated', task_id: 't1', column_id: 'c1' }], cards), { refresh: ['t1'], reload: false });
check('plan_should_refresh_a_commented_card_once',
  boardEventPlan([{ kind: 'comment_added', task_id: 't2', column_id: 'c2' }, { kind: 'task_updated', task_id: 't2', column_id: 'c2' }], cards),
  { refresh: ['t2'], reload: false });
check('plan_should_skip_a_move_this_page_made',
  boardEventPlan([{ kind: 'task_moved', task_id: 't1', column_id: 'c1', from_column_id: 'c2' }], cards), { refresh: [], reload: false });
check('plan_should_reload_for_a_foreign_move',
  boardEventPlan([{ kind: 'task_moved', task_id: 't1', column_id: 'c2', from_column_id: 'c1' }], cards), { refresh: [], reload: true });
check('plan_should_reload_for_an_added_task',
  boardEventPlan([{ kind: 'task_added', task_id: 't9', column_id: 'c1' }], cards), { refresh: [], reload: true });
check('plan_should_ignore_an_unknown_card',
  boardEventPlan([{ kind: 'task_updated', task_id: 'zz', column_id: 'c1' }, { kind: 'comment_added', task_id: 'zz', column_id: 'c1' }], cards),
  { refresh: [], reload: false });
check('plan_should_merge_refresh_and_reload',
  boardEventPlan([{ kind: 'task_updated', task_id: 't1', column_id: 'c1' }, { kind: 'task_added', task_id: 't9', column_id: 'c1' }], cards),
  { refresh: ['t1'], reload: true });
check('plan_should_give_nothing_for_an_empty_list', boardEventPlan([], cards), { refresh: [], reload: false });

if (failures) process.exit(1);
console.log('board_live: all checks passed');
