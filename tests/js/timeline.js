var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var TimelineDialog = (function () {');
var end = src.indexOf('\n  })();', start) + '\n  })();'.length;
if (start < 0 || end < start) throw new Error('TimelineDialog not found');
var document = { getElementById: function () { return null; } };
eval(src.slice(start, end));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
  else console.log('PASS ' + name);
}

check('a task with a future due date runs from today to it',
  TimelineDialog.defaultRange('2026-10-31', '2026-11-05'), ['2026-10-31', '2026-11-05']);
check('a task due today runs from today to today',
  TimelineDialog.defaultRange('2026-10-31', '2026-10-31'), ['2026-10-31', '2026-10-31']);
check('a task with no due date runs a week from today',
  TimelineDialog.defaultRange('2026-10-31', ''), ['2026-10-31', '2026-11-07']);
check('a past due date falls back to a week from today',
  TimelineDialog.defaultRange('2026-10-31', '2026-10-01'), ['2026-10-31', '2026-11-07']);
check('the week crosses a year end',
  TimelineDialog.defaultRange('2026-12-28', ''), ['2026-12-28', '2027-01-04']);

check('an end before the start is refused',
  TimelineDialog.problem('2026-11-07', '2026-10-31') !== null, true);
check('an end on the start is allowed', TimelineDialog.problem('2026-10-31', '2026-10-31'), null);
check('an end after the start is allowed', TimelineDialog.problem('2026-10-31', '2026-11-07'), null);
check('a missing date is refused', TimelineDialog.problem('', '2026-11-07') !== null, true);
check('a date that does not exist is refused',
  TimelineDialog.problem('2026-02-30', '2026-03-05') !== null, true);

check('the export wizard lists both Timeline columns',
  [src.indexOf("'Timeline start'") > -1, src.indexOf("'Timeline end'") > -1], [true, true]);
check('the export wizard fills both Timeline columns',
  [src.indexOf('timeline_start: t.timeline_start') > -1, src.indexOf('timeline_end: t.timeline_end') > -1],
  [true, true]);

process.exit(failures ? 1 : 0);
