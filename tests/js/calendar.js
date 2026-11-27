var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var CalendarPage = (function () {');
var end = src.indexOf('\n  })();', start) + '\n  })();'.length;
if (start < 0 || end < start) throw new Error('CalendarPage not found');
const elements = {};
function element(id) {
  if (!elements[id]) elements[id] = {
    value: '', checked: false, hidden: true, textContent: '', innerHTML: '', options: [], dataset: {},
    listeners: {},
    addEventListener(type, fn) { (this.listeners[type] = this.listeners[type] || []).push(fn); },
    setAttribute() {}, querySelectorAll: () => [],
    closest: () => null, focus() {}, remove() {},
    appendChild() {},
  };
  return elements[id];
}
const document = { getElementById: element, createElement: () => ({}) };
const window = { setTimeout: () => 0, clearTimeout() {}, location: { origin: 'https://x.test' } };
const settingsSaves = [];
let saveStatus = 200;
let boardData = { board: { name: 'One' }, tasks: [], columns: [], swimlanes: [], colors: [], users: [] };
function fetch(url, opts) {
  if (opts && opts.method === 'PUT') {
    settingsSaves.push(JSON.parse(opts.body));
    return Promise.resolve({ ok: saveStatus === 200, status: saveStatus });
  }
  return Promise.resolve({
    ok: true,
    json: () => Promise.resolve(boardData),
  });
}
const fetchJson = (url) => fetch(url, {}).then((res) => res.json());
const api = (path, method, data) => fetch(path, { method, body: data === undefined ? undefined : JSON.stringify(data) });
const toast = (message) => {
  const el = element('toast');
  el.textContent = message;
  el.hidden = false;
};
var accountPrefs = { hour12: true, weekStart: 0 };
function escapeHtml(s) { return String(s); }
function parseDueAt(s) { return new Date(s); }
eval(src.slice(start, end));

const RealDate = Date;
Date = class extends RealDate {
  constructor(...args) { super(...(args.length ? args : [2026, 9, 14, 12])); }
  static now() { return new RealDate(2026, 9, 14, 12).getTime(); }
};

const { check, finish } = require('./support/check.js');
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
function headers(html) { return (html.match(/role="columnheader">(\w+)</g) || []).map(function (h) { return h.replace(/^.*>(\w+)<$/, '$1'); }); }

var merged = CalendarPage.mergeBoards([
  { data: { board: { name: 'One' }, tasks: [task('a')], columns: [{ id: 'c1' }], swimlanes: [], colors: [{ id: 'k1' }], users: [{ id: 'u1' }] } },
  { data: { board: { name: 'Two' }, tasks: [task('b')], columns: [{ id: 'c2' }], swimlanes: [{ id: 's2' }], colors: [], users: [{ id: 'u1' }, { id: 'u2' }] } },
]);
check('boards merge into one dataset naming each task\'s board',
  [merged.tasks.map(function (t) { return t.id + '@' + t.board_name; }),
    merged.columns.length, merged.swimlanes.length, merged.colors.length,
    merged.users.map(function (u) { return u.id; })],
  [['a@One', 'b@Two'], 2, 1, 1, ['u1', 'u2']]);

const lanes = CalendarPage.mergeBoards([
  { data: { board: { name: 'One' }, tasks: [task('live', { swimlane_id: 'l1' }), task('frozen', { swimlane_id: 'l2' })], columns: [], swimlanes: [{ id: 'l1', archived: false }, { id: 'l2', archived: true }], colors: [], users: [] } },
]);
check('the calendar leaves out cards in archived lanes', lanes.tasks.map((t) => t.id), ['live']);
const mainArchived = CalendarPage.mergeBoards([
  { data: { board: { name: 'One', main_swimlane_id: 'l2' }, tasks: [task('live', { swimlane_id: 'l1' }), task('loose', { swimlane_id: null })], columns: [], swimlanes: [{ id: 'l1', archived: false }, { id: 'l2', archived: true }], colors: [], users: [] } },
]);
check('the calendar resolves a card with no lane to the main lane', mainArchived.tasks.map((t) => t.id), ['live']);
const epics = CalendarPage.mergeBoards([
  { data: { board: { name: 'One' }, tasks: [task('plain'), task('umbrella', { kind: 'epic' })], columns: [], swimlanes: [], colors: [], users: [] } },
]);
check('the calendar leaves epics out of the merged tasks', epics.tasks.map((t) => t.id), ['plain']);

check('the export link names the boards, format, range and type',
  CalendarPage.exportUrl(['b1', 'b2'], { format: 'csv', from: '2026-10-01', to: '2026-10-31', type: 'subtask' }),
  '/api/calendar-export?boards=b1%2Cb2&format=csv&from=2026-10-01&to=2026-10-31&type=subtask');
check('the export link defaults to Excel and all due dates',
  CalendarPage.exportUrl(['b1'], {}), '/api/calendar-export?boards=b1&format=xlsx&type=all');

var row = CalendarPage.feedRowHtml({ id: 'f1', label: 'Phone', path: '/api/calendar-feed/t.ics' }, 'https://x.test');
check('a feed row shows its label, URL and a delete button',
  [row.indexOf('Phone') > -1, row.indexOf('https://x.test/api/calendar-feed/t.ics') > -1,
    row.indexOf('data-feed-delete') > -1, row.indexOf('data-feed-id="f1"') > -1], [true, true, true, true]);

const flush = () => new Promise((resolve) => setImmediate(resolve));
const fire = (id, props) => {
  const el = element(id);
  Object.assign(el, props);
  el.listeners.change.forEach((fn) => fn());
};
const changeFirstDay = (value) => {
  fire('calendar-first-day', { value });
  return flush();
};
const setRepeats = (checked) => {
  fire('calendar-show-repeats', { checked });
  return flush();
};
const view = () => ({ html: element('calendar-grid').innerHTML, status: element('calendar-status').textContent });
const openCalendar = (tasks, extra) => {
  boardData = Object.assign(
    { board: { name: 'One' }, columns: [], swimlanes: [], colors: [], users: [] }, extra, { tasks });
  CalendarPage.open('b1');
  return flush().then(view);
};
const chipIds = (html) => (html.match(/data-task-id="[^"]*"/g) || []).map((m) => m.slice(14, -1));
const filterIds = ['calendar-user-filter', 'calendar-color-filter', 'calendar-label-filter', 'calendar-swimlane-filter'];
const applyFilters = (values) => {
  filterIds.forEach((id) => { fire(id, { value: values[id] || '' }); });
  const { html, status } = view();
  return [chipIds(html), status];
};

const filterChecks = async () => {
  const tasks = [
    task('a', { member_ids: ['u1'], color_id: 'red', labels: ['x'], swimlane_id: 'l1' }),
    task('b', { color_id: 'blue', swimlane_id: 'l2' }),
    task('c', { member_ids: ['u2'], labels: ['x'] }),
  ];
  await openCalendar(tasks, {
    swimlanes: [{ id: 'l1', name: 'One', archived: false }, { id: 'l2', name: 'Two', archived: false }],
    users: [{ id: 'u1', username: 'ann' }, { id: 'u2', username: 'bob' }],
  });
  const keeps = (name, values, ids) => {
    const count = ids.length === 1 ? '1 task with a due date in view.' : ids.length + ' tasks with due dates in view.';
    check('calendar_filter_should_keep_matching_tasks: ' + name, applyFilters(values), [ids, count]);
  };
  keeps('no filter keeps every task', {}, ['a', 'b', 'c']);
  keeps('the Unassigned user filter keeps only tasks without members', { 'calendar-user-filter': '__unassigned__' }, ['b']);
  keeps('a user filter keeps that user\'s tasks', { 'calendar-user-filter': 'u1' }, ['a']);
  keeps('a color filter keeps that color', { 'calendar-color-filter': 'red' }, ['a']);
  keeps('a label filter keeps that label', { 'calendar-label-filter': 'x' }, ['a', 'c']);
  keeps('a swimlane filter keeps that lane', { 'calendar-swimlane-filter': 'l2' }, ['b']);
  keeps('filters combine', { 'calendar-user-filter': 'u2', 'calendar-label-filter': 'x' }, ['c']);
  check('calendar_filter_should_keep_matching_tasks: a filter nothing matches says so',
    applyFilters({ 'calendar-color-filter': 'blue', 'calendar-label-filter': 'x' }),
    [[], 'No tasks with due dates in view.']);
  applyFilters({});
};

const repeatChecks = async () => {
  const weekly = task('w', { due_repeat: 'every week' });
  const plain = task('p');
  await setRepeats(true);
  let shown = await openCalendar([weekly, plain]);
  check('calendar_repeats_should_show_every_instance: the toggle on draws every instance',
    [chipIds(shown.html), shown.status], [['w', 'p', 'w', 'w', 'w'], '5 tasks with due dates in view.']);
  await setRepeats(false);
  shown = view();
  check('calendar_repeats_should_show_every_instance: the toggle off draws a repeating task once',
    [chipIds(shown.html), shown.status], [['w', 'p'], '2 tasks with due dates in view.']);
  const off = chipIds((await openCalendar([plain])).html);
  await setRepeats(true);
  const on = chipIds(view().html);
  check('calendar_repeats_should_show_every_instance: a task that does not repeat shows once either way',
    [off, on], [['p'], ['p']]);
  await setRepeats(false);
};

const chipCells = (html, id) => {
  const cells = html.split('role="gridcell"').slice(1);
  const hits = cells.map((cell, i) => (cell.indexOf('data-task-id="' + id + '"') > -1 ? i : -1)).filter((i) => i > -1);
  return [cells.length, hits];
};
const stepMonths = (months) => {
  const button = months < 0 ? 'calendar-prev' : 'calendar-next';
  for (let i = 0; i < Math.abs(months); i++) element(button).listeners.click.forEach((fn) => fn());
  return flush().then(() => view().html);
};

const monthEndChecks = async () => {
  await changeFirstDay('0');
  await setRepeats(true);
  const { html } = await openCalendar([task('m', { due_at: '2026-07-31T12:00:00', due_repeat: 'every month' })]);
  check('calendar_repeats_should_count_months_from_the_due_date: a monthly repeat due on the 31st sits on 31 October',
    chipCells(html, 'm'), [35, [3, 34]]);
  const leap = task('y', { due_at: '2024-02-29T12:00:00', due_repeat: 'every year' });
  await openCalendar([leap]);
  check('calendar_repeats_should_count_months_from_the_due_date: a yearly repeat due on 29 February sits on 28 February in a common year',
    chipCells(await stepMonths(-20), 'y'), [35, [33]]);
  await openCalendar([leap]);
  check('calendar_repeats_should_count_months_from_the_due_date: a yearly repeat due on 29 February sits on 29 February in a leap year',
    chipCells(await stepMonths(16), 'y'), [35, [30]]);
  await setRepeats(false);
};

const weekdayChecks = async () => {
  await changeFirstDay('0');
  const sunday = (await openCalendar([])).html;
  check('calendar_grid_should_start_on_the_chosen_weekday: the grid starts on Sunday when the week does',
    [headers(sunday)[0], sunday.split('cal-day"').length - 1 + sunday.split('cal-day cal-').length - 1], ['Sun', 35]);
  await changeFirstDay('1');
  const monday = view().html;
  check('calendar_grid_should_start_on_the_chosen_weekday: the grid starts on Monday when the week does',
    [headers(monday)[0], headers(monday)[6], monday.indexOf('>28<') < monday.indexOf('>29<')], ['Mon', 'Sun', true]);
  check('calendar_grid_should_start_on_the_chosen_weekday: the first week differs between the two starts',
    sunday === monday, false);
  await changeFirstDay('0');
};

const chipChecks = async () => {
  const { html } = await openCalendar([{ id: 't9', name: 'Pay', due_at: '2026-10-07T12:00:00', color_id: '', column_id: '' }]);
  check('calendar_chip_should_carry_its_task_id: a chip carries its task id and button role',
    [html.indexOf('data-task-id="t9"') > -1, html.indexOf('role="button"') > -1], [true, true]);
};

settingsSaves.length = 0;
filterChecks().then(repeatChecks).then(monthEndChecks).then(weekdayChecks).then(chipChecks).then(() => {
  settingsSaves.length = 0;
  return openCalendar([]);
}).then(() => {
  saveStatus = 500;
  return changeFirstDay('1');
}).then(() => {
  check('a failed save reverts the select', element('calendar-first-day').value, '0');
  check('a failed save says the setting could not be saved',
    [element('toast').hidden, element('toast').textContent], [false, 'Could not save the setting.']);
  check('a failed save renders the grid with the old first day', headers(element('calendar-grid').innerHTML)[0], 'Sun');
  saveStatus = 200;
  element('toast').hidden = true;
  element('toast').textContent = '';
  return changeFirstDay('1');
}).then(() => {
  check('a successful save keeps the new first day',
    [element('calendar-first-day').value, headers(element('calendar-grid').innerHTML)[0]], ['1', 'Mon']);
  check('a successful save shows no toast', [element('toast').hidden, element('toast').textContent], [true, '']);
  check('each save sends the week start', settingsSaves, [{ week_start: 'monday' }, { week_start: 'monday' }]);
  finish();
});
