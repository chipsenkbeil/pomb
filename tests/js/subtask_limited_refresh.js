const fs = require('fs');
const src = fs.readFileSync(process.argv[2], 'utf8');
const slice = (from, to) => {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
};

let failures = 0;
const check = (name, got, want) => {
  const g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
};
const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

const refreshed = [];
const TimerUI = { refreshTaskCard: (id) => { refreshed.push(id); } };
const toast = () => {};
const api = () => Promise.resolve({ ok: true });
let onChange = null;
const document = {
  addEventListener: (type, fn) => { if (type === 'change') onChange = fn; },
};
eval(slice('    // KF-478: folds', '    // Click a card to open its modal.'));

const tick = async (subAll) => {
  const li = { classList: { toggle: () => {} }, querySelector: () => null };
  const subsec = {
    dataset: { subAll },
    classList: { remove: () => {} },
    querySelector: () => null,
    querySelectorAll: (sel) => (sel === '.subTasks > li' ? [li] : []),
  };
  const card = {
    getAttribute: (name) => ({ 'data-task-id': 't1', 'data-subtasks-total': '3', 'data-subtasks-done': '0' })[name] || null,
    setAttribute: () => {},
    querySelector: (sel) => (sel === '.card-subsec' ? subsec : null),
  };
  const check = {
    checked: true,
    getAttribute: (name) => (name === 'data-subtask-id' ? 's1' : null),
    closest: (sel) => ({ '.card-sub-check': check, '.task-card': card, li: li }[sel] || null),
  };
  refreshed.length = 0;
  onChange({ target: check });
  await flush();
  return refreshed.slice();
};

(async () => {
  check('a limited section refreshes its card with the task id', await tick('false'), ['t1']);
  check('an all-subtasks section does not refresh its card', await tick('true'), []);
  if (failures) process.exit(1);
  console.log('subtask_limited_refresh ok');
})();
