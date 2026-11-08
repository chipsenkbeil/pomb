var src = require('fs').readFileSync(process.argv[2], 'utf8');
function block(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  var to = src.indexOf('\n  })();', from) + '\n  })();'.length;
  return src.slice(from, to);
}
function escapeHtml(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}
eval(block('  var ReportFilter = (function () {'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

var data = {
  users: [{ id: 'u1', username: 'ann' }, { id: 'u2', username: 'bob' }],
  colors: [{ id: 'c1', value: '#f00', label: 'Red' }, { id: 'c2', value: '#0f0', label: '' }],
  tasks: [
    { id: 'a', member_ids: ['u1'], color_id: 'c1', labels: ['x', 'y'] },
    { id: 'b', member_ids: ['u2'], color_id: 'c2', labels: ['x'] },
    { id: 'c', member_ids: [], color_id: null, labels: [] },
    { id: 'd', member_ids: ['u1', 'u2'], labels: ['y'] },
  ],
};
function ids(values) {
  return ReportFilter.filterTasks(data.tasks, values).map(function (t) { return t.id; });
}
var all = { user: '', color: '', labels: [], labelMode: 'and' };
function with_(patch) {
  var out = JSON.parse(JSON.stringify(all));
  Object.keys(patch).forEach(function (k) { out[k] = patch[k]; });
  return out;
}

check('no filter keeps every task', ids(all), ['a', 'b', 'c', 'd']);
check('user filter keeps tasks the user is on', ids(with_({ user: 'u1' })), ['a', 'd']);
check('unassigned keeps tasks with no member', ids(with_({ user: 'unassigned' })), ['c']);
check('color filter keeps that color', ids(with_({ color: 'c1' })), ['a']);
check('labels in all mode need every chosen label', ids(with_({ labels: ['x', 'y'] })), ['a']);
check('labels in any mode need one chosen label', ids(with_({ labels: ['x', 'y'], labelMode: ReportFilter.MODE_ANY })), ['a', 'b', 'd']);
check('filters combine', ids(with_({ user: 'u1', labels: ['y'] })), ['a', 'd']);

var controls = ReportFilter.taskControls(data);
check('control ids', controls.map(function (c) { return c.id; }), ['user', 'color', 'labels', 'labelMode']);
check('color without a label shows its value', controls[1].options[2].text, '#0f0');
check('labels are listed once, sorted', controls[2].options.map(function (o) { return o.value; }), ['x', 'y']);

var reloads = [];
var state = ReportFilter.create(controls, function (values) { reloads.push(values); });
state.set('user', 'u2');
state.set('labels', ['x']);
check('a changed control applies nothing', reloads.length, 0);
check('the applied values stay at their defaults', state.applied().user, '');
state.reload();
check('Reload applies once', reloads.length, 1);
check('Reload applies the draft', [reloads[0].user, reloads[0].labels], ['u2', ['x']]);
state.selectAll('labels');
check('Select all drafts every option', state.draft().labels, ['x', 'y']);
check('Select all applies nothing', state.applied().labels, ['x']);
state.clearAll('labels');
check('Clear all drafts none', state.draft().labels, []);

var handlers = {};
var host = {
  innerHTML: '',
  addEventListener: function (type, fn) { handlers[type] = fn; },
  querySelectorAll: function () { return []; },
};
var seen = [];
var wired = ReportFilter.create(controls, function (values) { seen.push(values.user); });
ReportFilter.mount(host, wired);
check('panel has a Reload button', /data-reload/.test(host.innerHTML), true);
check('panel has Select all and Clear all for labels',
  /data-select-all="labels"/.test(host.innerHTML) && /data-clear-all="labels"/.test(host.innerHTML), true);
handlers.change({ target: { getAttribute: function () { return 'user'; }, type: 'select-one', value: 'u1' } });
check('a select change applies nothing', seen.length, 0);
handlers.click({ target: { hasAttribute: function (a) { return a === 'data-reload'; } } });
check('the Reload button applies the change', seen, ['u1']);

var boxes = ['x', 'y'].map(function (v) {
  return { value: v, checked: false, getAttribute: function () { return 'labels'; } };
});
var host2 = {
  innerHTML: '',
  addEventListener: function (type, fn) { handlers[type] = fn; },
  querySelectorAll: function () { return boxes; },
};
var s2 = ReportFilter.create(controls, function () {});
ReportFilter.mount(host2, s2);
boxes[1].checked = true;
handlers.change({ target: { getAttribute: function () { return 'labels'; }, type: 'checkbox' } });
check('a checkbox change drafts the checked values', s2.draft().labels, ['y']);
handlers.click({ target: { hasAttribute: function (a) { return a === 'data-select-all'; },
  getAttribute: function () { return 'labels'; } } });
check('Select all ticks every box', boxes.map(function (b) { return b.checked; }), [true, true]);
handlers.click({ target: { hasAttribute: function (a) { return a === 'data-clear-all'; },
  getAttribute: function () { return 'labels'; } } });
check('Clear all unticks every box', boxes.map(function (b) { return b.checked; }), [false, false]);

var odd = ReportFilter.taskControls({ users: [], colors: [], tasks: [{ labels: ['a"b<'] }] });
var host3 = { innerHTML: '', addEventListener: function () {}, querySelectorAll: function () { return []; } };
ReportFilter.mount(host3, ReportFilter.create(odd, function () {}));
check('option values are escaped', /value="a&quot;b&lt;"/.test(host3.innerHTML), true);
var none = { innerHTML: '', addEventListener: function () {}, querySelectorAll: function () { return []; } };
ReportFilter.mount(none, ReportFilter.create(ReportFilter.taskControls({ tasks: [] }), function () {}));
check('no labels shows an empty note and no Select all', /rf-empty/.test(none.innerHTML) && !/data-select-all/.test(none.innerHTML), true);

var failed = [];
var opened = ReportFilter.open(null, controls, function () { throw new Error('boom'); },
  function (err) { failed.push(err.message); });
check('a render error is reported', failed, ['boom']);

var applied = [];
var bookmarked = ReportFilter.open(null, controls, function (values) { applied.push(values); }, function () {});
bookmarked.apply({ user: 'u2', labels: ['x', 'gone'], labelMode: 'nope', color: 5, unknown: 'ignored' });
check('apply reloads once', applied.length, 2);
check('apply sets a known option', applied[1].user, 'u2');
check('apply drops options the board no longer has', applied[1].labels, ['x']);
check('apply keeps the value of an unknown option out', applied[1].labelMode, 'and');
check('apply ignores a value of the wrong type', applied[1].color, '');
check('apply ignores a key no control has', 'unknown' in applied[1], false);

var redraws = 0;
var host4 = { innerHTML: '', addEventListener: function () {}, querySelectorAll: function () { return []; } };
var drawn = ReportFilter.create(controls, function () {});
ReportFilter.mount(host4, drawn);
var before = host4.innerHTML;
drawn.apply({ user: 'u1' });
check('apply redraws the panel with the new value', host4.innerHTML !== before && /value="u1" selected/.test(host4.innerHTML), true);

if (failures) process.exit(1);
console.log('report_filter: all checks passed');
