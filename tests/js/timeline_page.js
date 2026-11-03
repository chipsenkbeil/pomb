var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  const TimelinePage = (() => {');
var end = src.indexOf('\n  })();', start) + '\n  })();'.length;
if (start < 0 || end < start) throw new Error('TimelinePage not found');
var TimelinePage;
eval(src.slice(start, end).replace('const TimelinePage', 'TimelinePage'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
  else console.log('PASS ' + name);
}

var day = TimelinePage.isoDay('2026-11-03');
check('a bad date has no day', TimelinePage.isoDay('2026-1-3'), null);
check('days count up by one', TimelinePage.isoDay('2026-11-04') - day, 1);
check('days count across a clock change', TimelinePage.isoDay('2026-11-02') - TimelinePage.isoDay('2026-10-31'), 2);
check('an empty timeline spans the lead, today and 45 days',
  TimelinePage.rangeOf([], day), { first: day - 7, last: day - 7 + 44 });
check('a bar and its due date widen the range',
  TimelinePage.rangeOf([{ start: day - 20, end: day + 90, due: day + 100 }], day),
  { first: day - 27, last: day + 114 });
var bar = { start: day, end: day + 3 };
check('a move shifts both ends', TimelinePage.shifted(bar, 'move', 2), { start: day + 2, end: day + 5 });
check('an end resize moves the end', TimelinePage.shifted(bar, 'end', 2), { start: day, end: day + 5 });
check('a start resize moves the start', TimelinePage.shifted(bar, 'start', -2), { start: day - 2, end: day + 3 });
check('the end stops at the start', TimelinePage.shifted(bar, 'end', -9), { start: day, end: day });
check('the start stops at the end', TimelinePage.shifted(bar, 'start', 9), { start: day + 3, end: day + 3 });
check('a day writes as an ISO date', TimelinePage.isoOf(day + 28), '2026-12-01');
check('zoom stops at 30', TimelinePage.clampZoom(10), 30);
check('zoom stops at 80', TimelinePage.clampZoom(200), 80);
check('zoom keeps a value between', TimelinePage.clampZoom('55'), 55);
check('zoom falls back on junk', TimelinePage.clampZoom('x'), 30);
check('epics stay off the timeline',
  TimelinePage.cardsOf([{ id: 'a', kind: 'task' }, { id: 'e', kind: 'epic' }, { id: 'b' }]).map((task) => task.id),
  ['a', 'b']);
check('no tasks is an empty list', TimelinePage.cardsOf(undefined), []);
if (failures) process.exit(1);
