var src = require('fs').readFileSync(process.argv[2], 'utf8');
var from = src.indexOf('  var LocalTime = (function () {');
var to = src.indexOf('\n  })();\n', from);
if (from < 0 || to < 0) { console.log('FAIL not found: LocalTime'); process.exit(1); }
var accountPrefs = { hour12: true };
var LocalTime;
eval(src.slice(from, to + '\n  })();\n'.length));

var failures = 0;
function check(name, got, want) {
  if (got !== want) { failures++; console.log('FAIL ' + name + ': got ' + JSON.stringify(got) + ' want ' + JSON.stringify(want)); }
}

var chicago = 'America/Chicago';
var due = Date.parse('2026-10-01T20:30:00Z');
var now = Date.parse('2026-10-01T14:00:00Z');
check('card 12h', LocalTime.write('due-card', due, now, chicago, true), 'Today 3:30 PM');
check('card 24h', LocalTime.write('due-card', due, now, chicago, false), 'Today 15:30');
check('modal 12h', LocalTime.write('due-modal', due, now, chicago, true), 'Today 3:30 PM');
check('datetime 12h', LocalTime.write('datetime', due, now, chicago, true), 'Oct 01, 2026 3:30 PM');
check('datetime 24h', LocalTime.write('datetime', due, now, chicago, false), 'Oct 01, 2026 15:30');
check('clock midnight 24h', LocalTime.write('clock', Date.parse('2026-10-01T05:05:00Z'), now, chicago, false), '00:05');
check('clock midnight 12h', LocalTime.write('clock', Date.parse('2026-10-01T05:05:00Z'), now, chicago, true), '12:05 AM');

var late = Date.parse('2026-10-02T02:30:00Z');
check('zone moves the day back', LocalTime.write('due-card', late, now, chicago, true), 'Today 9:30 PM');
check('utc keeps the day', LocalTime.write('due-card', late, now, 'UTC', true), 'Friday 2:30 AM');
check('weekday', LocalTime.write('due-card', Date.parse('2026-10-03T20:30:00Z'), now, chicago, true), 'Saturday 3:30 PM');
check('modal other day', LocalTime.write('due-modal', Date.parse('2026-10-20T20:30:00Z'), now, chicago, true), 'Oct 20 at 3:30 PM');
check('past is absolute', LocalTime.write('due-card', Date.parse('2026-09-25T20:30:00Z'), now, chicago, true), '25 September 3:30 PM');
check('comment yesterday', LocalTime.write('comment', Date.parse('2026-09-30T20:30:00Z'), now, chicago, true), 'Yesterday 3:30 PM');
check('comment older', LocalTime.write('comment', Date.parse('2026-09-27T20:30:00Z'), now, chicago, true), 'Sep 27 3:30 PM');
check('span', LocalTime.writeSpan(due, due + 25 * 60000, now, chicago, true), '3:30 PM - 3:55 PM');

function element(attrs) {
  var el = { attrs: attrs, textContent: 'Today 8:30 PM UTC' };
  el.getAttribute = function (name) { return name in this.attrs ? this.attrs[name] : null; };
  el.setAttribute = function (name, value) { this.attrs[name] = value; };
  el.matches = function () { return !('data-local' in this.attrs); };
  el.querySelectorAll = function () { return []; };
  return el;
}
var el = element({ 'data-at': '2026-10-01T20:30:00Z', 'data-fmt': 'datetime', 'data-clock': '24h' });
LocalTime.apply(el);
check('apply marks the element', 'data-local' in el.attrs, true);
var bad = element({ 'data-at': 'nope', 'data-fmt': 'datetime' });
LocalTime.apply(bad);
check('apply leaves a bad instant', bad.textContent, 'Today 8:30 PM UTC');
check('apply marks a bad instant so it shows', 'data-local' in bad.attrs, true);
var titled = element({ 'data-at': '2026-10-01T20:30:00Z', 'data-fmt': 'datetime', 'data-attr': 'title', 'data-clock': '12h' });
LocalTime.apply(titled);
check('apply sets an attribute', /^[A-Z][a-z]{2} \d\d, 2026 /.test(titled.attrs.title), true);
check('apply keeps the text for an attribute', titled.textContent, 'Today 8:30 PM UTC');

if (failures) process.exit(1);
console.log('ok');
