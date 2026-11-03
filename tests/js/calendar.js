var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var CalendarPage = (function () {');
var end = src.indexOf('\n  })();', start) + '\n  })();'.length;
if (start < 0 || end < start) throw new Error('CalendarPage not found');
var document = { getElementById: function () { return null; } };
var accountPrefs = { hour12: true, weekStart: 0 };
function escapeHtml(s) { return String(s); }
function parseDueAt(s) { return new Date(s); }
eval(src.slice(start, end));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
  else console.log('PASS ' + name);
}
function ymd(d) { return d.getFullYear() + '-' + (d.getMonth() + 1) + '-' + d.getDate(); }

check('headers start on Sunday by default',
  CalendarPage.weekdayNames(0), ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat']);
check('headers rotate for a Monday start',
  CalendarPage.weekdayNames(1), ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun']);

var sun = CalendarPage.gridRange(2026, 9, 0);
check('October 2026 grid from Sunday', [ymd(sun.start), ymd(sun.end)], ['2026-9-27', '2026-10-31']);
var mon = CalendarPage.gridRange(2026, 9, 1);
check('October 2026 grid from Monday', [ymd(mon.start), ymd(mon.end)], ['2026-9-28', '2026-11-1']);
check('grid spans whole weeks',
  [(sun.end - sun.start) / 86400000 % 7, (mon.end - mon.start) / 86400000 % 7].map(Math.round), [6, 6]);

check('the first of a month names the month',
  [CalendarPage.dayLabel(new Date(2026, 10, 1)), CalendarPage.dayLabel(new Date(2026, 9, 1)),
    CalendarPage.dayLabel(new Date(2026, 9, 14))], ['November 1', 'October 1', '14']);

var today = new Date(2026, 9, 14);
var todayHtml = CalendarPage.dayCellHtml(today, 9, '2026-10-14', '');
check('today carries the cal-today class and the Today label',
  [todayHtml.indexOf('cal-today') > -1, todayHtml.indexOf('>Today<') > -1], [true, true]);
var plain = CalendarPage.dayCellHtml(new Date(2026, 9, 15), 9, '2026-10-14', '');
check('other days have neither',
  [plain.indexOf('cal-today') > -1, plain.indexOf('Today') > -1], [false, false]);
var outside = CalendarPage.dayCellHtml(new Date(2026, 10, 1), 9, '2026-10-14', '');
check('outside days are marked and name the month',
  [outside.indexOf('cal-outside') > -1, outside.indexOf('November 1') > -1], [true, true]);
var inside = CalendarPage.dayCellHtml(new Date(2026, 9, 15), 9, '2026-10-14', '');
check('inside days are not marked outside', inside.indexOf('cal-outside') > -1, false);

function moved(from, to) {
  return { kind: 'moved', detail: 'Moved from ' + from + ' to ' + to, created_at: '2026-10-01T00:00:00Z' };
}
check('chip line shows the column the task moved from and to',
  CalendarPage.moveLine({ history: [moved('Backlog', 'Done')] }, 'Done'), 'Backlog → Done');
check('chip line uses the latest move',
  CalendarPage.moveLine({ history: [moved('Backlog', 'Doing'), moved('Doing', 'Done')] }, 'Done'),
  'Doing → Done');
check('chip line keeps the from column when it holds " to "',
  CalendarPage.moveLine({ history: [moved('Back to work', 'Done')] }, 'Done'), 'Back to work → Done');
check('chip line ignores the dropped fields note',
  CalendarPage.moveLine({ history: [{ kind: 'moved', detail: 'Moved from A to B; dropped fields: {"x":" to y"}' }] }, 'B'),
  'A → B');
check('chip line is the column alone for a task that never moved',
  CalendarPage.moveLine({ history: [{ kind: 'created', detail: 'Task created in Backlog' }] }, 'Backlog'),
  'Backlog');
function task(id, extra) {
  var t = { id: id, name: id, due_at: '2026-10-07T12:00:00', member_ids: [], labels: [], color_id: '', swimlane_id: '' };
  Object.keys(extra || {}).forEach(function (k) { t[k] = extra[k]; });
  return t;
}
var none = { user: '', color: '', label: '', swimlane: '' };
function withFilter(extra) {
  var f = {};
  Object.keys(none).forEach(function (k) { f[k] = none[k]; });
  Object.keys(extra).forEach(function (k) { f[k] = extra[k]; });
  return f;
}

check('the Unassigned user filter keeps only tasks without members',
  [CalendarPage.taskPasses(task('a'), withFilter({ user: '__unassigned__' })),
    CalendarPage.taskPasses(task('b', { member_ids: ['u1'] }), withFilter({ user: '__unassigned__' }))],
  [true, false]);
check('a user filter keeps that user\'s tasks',
  [CalendarPage.taskPasses(task('a', { member_ids: ['u1'] }), withFilter({ user: 'u1' })),
    CalendarPage.taskPasses(task('b'), withFilter({ user: 'u1' }))], [true, false]);
check('color and label filters narrow tasks',
  [CalendarPage.taskPasses(task('a', { color_id: 'red' }), withFilter({ color: 'red' })),
    CalendarPage.taskPasses(task('b', { color_id: 'blue' }), withFilter({ color: 'red' })),
    CalendarPage.taskPasses(task('c', { labels: ['x'] }), withFilter({ label: 'x' })),
    CalendarPage.taskPasses(task('d'), withFilter({ label: 'x' }))], [true, false, true, false]);

var oct = CalendarPage.gridRange(2026, 9, 0);
var weekly = task('w', { due_repeat: 'every week' });
var shownWith = CalendarPage.bucketTasks([weekly], oct.start, oct.end, true, none).shown;
var shownWithout = CalendarPage.bucketTasks([weekly], oct.start, oct.end, false, none).shown;
check('repeating tasks show every instance in view', shownWith, 4);
check('the repeating toggle off hides the repeats', shownWithout, 1);
check('a task that does not repeat shows once either way',
  [CalendarPage.bucketTasks([task('p')], oct.start, oct.end, true, none).shown,
    CalendarPage.bucketTasks([task('p')], oct.start, oct.end, false, none).shown], [1, 1]);

var sundayGrid = CalendarPage.gridHtml([], 2026, 9, 0, true, none, '2026-10-14').html;
var mondayGrid = CalendarPage.gridHtml([], 2026, 9, 1, true, none, '2026-10-14').html;
function headers(html) { return (html.match(/role="columnheader">(\w+)</g) || []).map(function (h) { return h.replace(/^.*>(\w+)<$/, '$1'); }); }
check('the grid starts on Sunday when the week does',
  [headers(sundayGrid)[0], sundayGrid.split('cal-day"').length - 1 + sundayGrid.split('cal-day cal-').length - 1], ['Sun', 35]);
check('the grid starts on Monday when the week does',
  [headers(mondayGrid)[0], headers(mondayGrid)[6], mondayGrid.indexOf('>28<') < mondayGrid.indexOf('>29<')], ['Mon', 'Sun', true]);
check('the first week differs between the two starts', sundayGrid === mondayGrid, false);
var repeatGrid = function (repeats) {
  return CalendarPage.gridHtml([weekly], 2026, 9, 0, repeats, none, '2026-10-14').html.split('class="cal-chip"').length - 1;
};
check('the grid draws the repeats only when shown', [repeatGrid(true), repeatGrid(false)], [4, 1]);

var merged = CalendarPage.mergeBoards([
  { data: { board: { name: 'One' }, tasks: [task('a')], columns: [{ id: 'c1' }], swimlanes: [], colors: [{ id: 'k1' }], users: [{ id: 'u1' }] } },
  { data: { board: { name: 'Two' }, tasks: [task('b')], columns: [{ id: 'c2' }], swimlanes: [{ id: 's2' }], colors: [], users: [{ id: 'u1' }, { id: 'u2' }] } }
]);
check('boards merge into one dataset naming each task\'s board',
  [merged.tasks.map(function (t) { return t.id + '@' + t.board_name; }),
    merged.columns.length, merged.swimlanes.length, merged.colors.length,
    merged.users.map(function (u) { return u.id; })],
  [['a@One', 'b@Two'], 2, 1, 1, ['u1', 'u2']]);

check('the export link names the boards, format, range and type',
  CalendarPage.exportUrl(['b1', 'b2'], { format: 'csv', from: '2026-10-01', to: '2026-10-31', type: 'subtask' }),
  '/api/calendar-export?boards=b1%2Cb2&format=csv&from=2026-10-01&to=2026-10-31&type=subtask');
check('the export link defaults to Excel and all due dates',
  CalendarPage.exportUrl(['b1'], {}), '/api/calendar-export?boards=b1&format=xlsx&type=all');

var row = CalendarPage.feedRowHtml({ id: 'f1', label: 'Phone', path: '/api/calendar-feed/t.ics' }, 'https://x.test');
check('a feed row shows its label, URL and a delete button',
  [row.indexOf('Phone') > -1, row.indexOf('https://x.test/api/calendar-feed/t.ics') > -1,
    row.indexOf('data-feed-delete') > -1, row.indexOf('data-feed-id="f1"') > -1], [true, true, true, true]);

var chip = CalendarPage.renderChip({ id: 't9', name: 'Pay', color_id: '', column_id: '' }, null);
check('a chip carries its task id so a click can open the task',
  [chip.indexOf('data-task-id="t9"') > -1, chip.indexOf('role="button"') > -1], [true, true]);

process.exit(failures ? 1 : 0);
