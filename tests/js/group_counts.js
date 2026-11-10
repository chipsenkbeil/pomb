const src = require('fs').readFileSync(process.argv[2], 'utf8');
const start = src.indexOf('  const refreshGroupCounts = ');
const end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('refreshGroupCounts not found');

const header = (groupId, limit, taskCount) => ({
  dataset: { groupId, groupLimit: String(limit), taskCount: String(taskCount) },
});

const band = (groupId) => {
  const classes = new Set();
  return {
    dataset: { groupId },
    textContent: '',
    classList: { toggle: (name, on) => (on ? classes.add(name) : classes.delete(name)) },
    classes,
  };
};

const load = (headers, bands) => {
  const document = {
    querySelectorAll: (sel) => {
      if (sel === '.columnHeader[data-group-id]') return headers;
      if (sel === '.columnHeader-groupBand[data-group-id]') return bands;
      return [];
    },
  };
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
const band1 = band('g1');
const band2 = band('g2');
const refresh = load([a, b, other], [band1, band2]);
refresh();
check('a group sums its columns', band1.textContent, '2 / 3');
check('under the limit has no over class', band1.classes.has('columnHeader-groupBand--over'), false);
check('another group stays apart', band2.textContent, '1 / 1');

b.dataset.taskCount = '3';
refresh();
check('a drag into the group raises the band', band1.textContent, '4 / 3');
check('over the limit sets the over class', band1.classes.has('columnHeader-groupBand--over'), true);
check('the other group is not over', band2.classes.has('columnHeader-groupBand--over'), false);

b.dataset.taskCount = '2';
refresh();
check('a drag out clears the over class', band1.classes.has('columnHeader-groupBand--over'), false);

if (failures) process.exit(1);
console.log('group counts ok');
