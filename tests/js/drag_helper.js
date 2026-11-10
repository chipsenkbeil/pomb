var src = require('fs').readFileSync(process.argv[2], 'utf8');
function slice(from, to) {
  var start = src.indexOf(from);
  var end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
}

var selectedIds = [];
var helpers = [];
var labels = [];
var timeouts = [];
var window = { setTimeout: function (fn) { timeouts.push(fn); } };
var document = {
  body: { appendChild: function (el) { helpers.push(el); } },
  createElement: function () { return { className: '', textContent: '', remove: function () { this.removed = true; } }; },
  querySelectorAll: function (sel) { return sel === '.done-group-label--target' ? labels.filter(function (l) { return l.marked; }) : []; },
};
function isSelected(card) { return selectedIds.indexOf(card.dataset.taskId) !== -1; }
function selectedCards() { return selectedIds.map(function (id) { return { dataset: { taskId: id } }; }); }
eval(slice('  function dragHelper(card) {', '  function reloadSelectionSoon'));

function label(text) {
  var l = {
    text: text, marked: false,
    dataset: { doneToday: text === 'Today' },
    textContent: ' ' + text + ' ',
    classList: {
      add: function () { l.marked = true; },
      remove: function () { l.marked = false; },
    },
  };
  labels.push(l);
  return l;
}
function list(done, groups) {
  return {
    dataset: { doneColumn: done ? 'true' : 'false' },
    querySelectorAll: function () { return groups; },
    querySelector: () => groups.filter((g) => g.dataset.doneToday)[0],
  };
}

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

var card = { dataset: { taskId: 't1', taskName: 'Write the docs' } };
var single = dragHelper(card);
check('a single drag helper reads the task name only', [single.textContent, single.className], ['Write the docs', 'drag-helper']);

selectedIds = ['t1', 't2', 't3'];
var group = dragHelper(card);
check('a group drag helper reads Move N tasks', group.textContent, 'Move 3 tasks');
check('a group drag helper has the group class', group.className, 'drag-helper drag-helper--group');
check('the helper stays in the page until the drag image is taken', [helpers.length, group.removed], [2, undefined]);
timeouts.forEach(function (fn) { fn(); });
check('the helper is removed after the drag image is taken', [single.removed, group.removed], [true, true]);

var today = label('Today');
var earlier = label('Yesterday');
markDoneTarget(list(true, [earlier, today]));
check('a held card over Done underlines the Today label', [today.marked, earlier.marked], [true, false]);
markDoneTarget(list(false, [earlier, today]));
check('a held card over another column underlines nothing', [today.marked, earlier.marked], [false, false]);
markDoneTarget(list(true, [earlier, today]));
markDoneTarget(null);
check('the end of the drag clears the underline', today.marked, false);

if (failures) process.exit(1);
console.log('drag_helper ok');
