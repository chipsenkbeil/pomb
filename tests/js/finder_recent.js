var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('    recent: function () {');
var end = src.indexOf('    activate: function (highlight) {', start);
if (start < 0 || end < start) throw new Error('finder recent methods not found');

var store = {};
var failWrites = false;
var window = {
  localStorage: {
    getItem: function (k) { return k in store ? store[k] : null; },
    setItem: function (k, v) {
      if (failWrites) throw new Error('quota');
      store[k] = v;
    }
  }
};
var Finder = eval('({ RECENT_KEY: "k", RECENT_MAX_ENTRIES: 2, MATCH: { TITLE: "title" },\n' +
  src.slice(start, end).replace(/,\s*$/, '') + '\n})');

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function ids() { return JSON.parse(store.k).map(function (e) { return e.task_id; }); }

check('the finder has one recent-list save helper', typeof Finder.saveRecent, 'function');

Finder.remember({ task_id: 'a', name: 'A' });
Finder.remember({ task_id: 'b', name: 'B' });
Finder.remember({ task_id: 'c', name: 'C' });
check('remember keeps the newest entries up to the cap', ids(), ['c', 'b']);

Finder.remember({ task_id: 'b', name: 'B' });
check('remember moves a repeated card to the front', ids(), ['b', 'c']);

Finder.forget('b');
check('forget drops the card from the list', ids(), ['c']);

failWrites = true;
Finder.remember({ task_id: 'd', name: 'D' });
Finder.forget('c');
failWrites = false;
check('a failed write leaves the stored list as it was', ids(), ['c']);

if (failures) process.exit(1);
console.log('finder_recent ok');
