const src = require('fs').readFileSync(process.argv[2], 'utf8');
const start = src.indexOf('  const refreshGroupCounts = ');
const end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('refreshGroupCounts not found');

const header = (groupId, limit, taskCount) => {
  const classes = new Set();
  const span = {
    textContent: '',
    classList: { toggle: (name, on) => (on ? classes.add(name) : classes.delete(name)) },
  };
  return {
    dataset: { groupId, groupLimit: String(limit), taskCount: String(taskCount) },
    querySelector: (sel) => (sel === '.columnHeader-groupCount' ? span : null),
    span,
    classes,
  };
};

const load = (headers) => {
  const document = { querySelectorAll: (sel) => (sel === '.columnHeader[data-group-id]' ? headers : []) };
  return eval('(function () {' + src.slice(start, end) + '\n return refreshGroupCounts; })()');
};

let failures = 0;
const check = (name, got, want) => {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g !== w) {
    failures++;
    console.log('FAIL ' + name + ': got ' + g + ' want ' + w);
  }
};

const a = header('g1', 3, 1);
const b = header('g1', 3, 1);
const other = header('g2', 1, 1);
const refresh = load([a, b, other]);
refresh();
check('a group sums its columns', [a.span.textContent, b.span.textContent], ['2 / 3', '2 / 3']);
check('under the limit has no over class', a.classes.has('columnHeader-groupCount--over'), false);
check('another group stays apart', other.span.textContent, '1 / 1');

b.dataset.taskCount = '3';
refresh();
check('a drag into the group raises every span', [a.span.textContent, b.span.textContent], ['4 / 3', '4 / 3']);
check('over the limit sets the over class on each', [a.classes.has('columnHeader-groupCount--over'), b.classes.has('columnHeader-groupCount--over')], [true, true]);
check('the other group is not over', other.classes.has('columnHeader-groupCount--over'), false);

b.dataset.taskCount = '2';
refresh();
check('a drag out clears the over class', a.classes.has('columnHeader-groupCount--over'), false);

if (failures) process.exit(1);
console.log('group counts ok');
