var src = require('fs').readFileSync(process.argv[2], 'utf8');
var from = src.indexOf('  var BoardHistoryPage = (function () {');
var to = src.indexOf('  // Calendar report: tasks laid out by due date', from);
if (from < 0 || to < 0) throw new Error('page scripts not found');
var BoardHistoryPage, PrintBoard;
function escapeHtml(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}
var document = {};
eval(src.slice(from, to));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

var board = {
  board: { name: 'Work <1>' },
  columns: [
    { id: 'c1', name: 'Todo', is_done: false },
    { id: 'c2', name: 'Shipped', is_done: true },
  ],
  colors: [{ id: 'k1', value: 'yellow', label: 'Urgent' }, { id: 'k2', value: 'blue', label: 'Plain' }],
  users: [{ id: 'u1', username: 'ann' }, { id: 'u2', username: 'bob' }],
  tasks: [
    { id: 't1', name: 'First', column_id: 'c1', position: 2, color_id: 'k1', member_ids: ['u1'],
      labels: ['api'], description: 'Write <it>', estimate_hours: 1.5, total_minutes: 75,
      timeline_start: '2026-10-01', timeline_end: '2026-10-03',
      subtasks: [{ name: 'a', done: true }, { name: 'b', done: false }],
      comments: [{ author: 'bob', body: 'hi' }],
      links: [{ kind: 'blocked_by', target_id: 't2' }] },
    { id: 't2', name: 'Second', column_id: 'c1', position: 1, color_id: 'k2', member_ids: [], labels: [] },
    { id: 't3', name: 'Old done', column_id: 'c2', position: 1, member_ids: ['u2'], labels: ['api'],
      completed_at: '2026-01-05T10:00:00Z' },
    { id: 't4', name: 'New done', column_id: 'c2', position: 2, member_ids: [], labels: [],
      completed_at: '2026-10-27T10:00:00Z' },
  ],
};
var now = new Date(2026, 9, 28, 12, 0);
function names(filters) {
  var f = { columns: ['c1', 'c2'], props: [], user: '', color: '', label: '', period: 'all' };
  Object.keys(filters).forEach(function (k) { f[k] = filters[k]; });
  return PrintBoard.selectTasks(board, f, now).map(function (t) { return t.name; });
}

check('tasks list by column then position', names({}), ['Second', 'First', 'Old done', 'New done']);
check('columns narrow the sheet', names({ columns: ['c2'] }), ['Old done', 'New done']);
check('user narrows', names({ user: 'u2' }), ['Old done']);
check('unassigned narrows', names({ user: '__unassigned__' }), ['Second', 'New done']);
check('color narrows', names({ color: 'k1' }), ['First']);
check('label narrows', names({ label: 'api' }), ['First', 'Old done']);
check('period keeps open tasks and recent done tasks', names({ period: 'this-month' }), ['Second', 'First', 'New done']);

var sheet = PrintBoard.sheetHtml(board, {
  columns: ['c1'], props: ['description', 'color', 'members', 'estimate', 'spent', 'labels',
    'timeline', 'subtasks', 'comments', 'relations'],
  user: '', color: '', label: '', period: 'all',
}, now);
check('one block per task', (sheet.match(/<section class="print-task">/g) || []).length, 2);
check('the board name is escaped', sheet.indexOf('Work &lt;1&gt;') >= 0, true);
check('the description is escaped', sheet.indexOf('Write &lt;it&gt;') >= 0, true);
check('estimate and spent', sheet.indexOf('1h 30m') >= 0 && sheet.indexOf('1h 15m') >= 0, true);
check('relation names the other task', sheet.indexOf('Depends on Second') >= 0, true);
check('a date-only timeline prints as stored', sheet.indexOf('2026-10-01 to 2026-10-03') >= 0, true);
check('members and color', sheet.indexOf('ann') >= 0 && sheet.indexOf('Urgent') >= 0, true);

var bare = PrintBoard.sheetHtml(board, {
  columns: ['c1'], props: [], user: '', color: '', label: '', period: 'all',
}, now);
check('a block holds only the chosen properties', bare.indexOf('<dl>') < 0, true);
check('an empty result says so', PrintBoard.sheetHtml(board, {
  columns: [], props: [], user: '', color: '', label: '', period: 'all',
}, now).indexOf('No tasks match the filters.') >= 0, true);

console.log('failures=' + failures);
process.exit(failures ? 1 : 0);
