var src = require('fs').readFileSync(process.argv[2], 'utf8');
function slice(from, to) {
  var start = src.indexOf(from);
  var end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
}

var clicks = [];
var calls = [];
var focused = [];
var els = {};
var addButtons = [];
var floatingOpen = false;
var BoardChrome = {
  openFilter: function () { calls.push('filter'); return true; },
  openReports: function () { calls.push('reports'); return true; },
  showAllMembers: function () { calls.push('members'); return true; },
  openHelp: function () { calls.push('help'); return true; }
};
var TimerUI = {
  switchModeTab: function () { calls.push('mode'); },
  togglePopup: function () { calls.push('toggle'); },
  beginSelect: function () { calls.push('select'); },
  runKey: function () { calls.push('run'); return true; }
};

function makeAdd(columnId) {
  var header = { children: [], appendChild: function (c) { this.children.push(c); c.parent = this; } };
  return {
    columnId: columnId,
    header: header,
    click: function () { clicks.push(columnId); },
    closest: function () { return header; }
  };
}
for (var c = 1; c <= 11; c++) addButtons.push(makeAdd('col' + c));

var document = {
  createElement: function () {
    return { className: '', textContent: '', remove: function () { this.parent.children.splice(this.parent.children.indexOf(this), 1); } };
  },
  querySelectorAll: function (sel) { return /columnHeader-add/.test(sel) ? addButtons : []; },
  querySelector: function (sel) { return /dlg-overlay/.test(sel) && floatingOpen ? {} : null; },
  getElementById: function (id) { return els[id] || null; },
  activeElement: null
};
eval(slice('  var columnKeyHints = [];', '  var MODAL_LETTER_ACTS'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function press(k, shift) {
  var event = { key: k, shiftKey: !!shift, prevented: false, preventDefault: function () { this.prevented = true; } };
  var handled = onBoardShortcut(event, k.toLowerCase());
  return { handled: handled, prevented: event.prevented };
}
function hintTexts() {
  return addButtons.map(function (a) { return a.header.children.map(function (h) { return h.textContent; }); })
    .filter(function (t) { return t.length; }).map(function (t) { return t[0]; });
}

check('a numbers the first nine columns', (press('a'), hintTexts()), ['1', '2', '3', '4', '5', '6', '7', '8', '9']);
var digit = press('3');
check('a then 3 opens the add dialog for the third column', [clicks, digit.handled, digit.prevented], [['col3'], true, true]);
check('a digit clears the numbers', hintTexts(), []);

press('a');
check('Esc clears the numbers', [clearColumnHints(), hintTexts()], [true, []]);
check('Esc with no numbers is left to the next handler', clearColumnHints(), false);

press('a');
press('x');
check('another key cancels the pending add', hintTexts(), []);
clicks.length = 0;
press('a');
press('2');
press('2');
check('a digit after the pick does nothing', clicks, ['col2']);

addButtons.length = 2;
press('a');
press('7');
check('a digit past the last column opens nothing', [clicks, hintTexts()], [['col2'], []]);
addButtons.length = 0;
check('a on a board with no column buttons is not handled', press('a').handled, false);

els['help-dialog'] = {};
calls.length = 0;
[['f', 'filter'], ['r', 'reports'], ['u', 'members'], ['h', 'help'], ['s', 'run']].forEach(function (pair) {
  var out = press(pair[0]);
  check(pair[0] + ' runs ' + pair[1], [calls.pop(), out.handled, out.prevented], [pair[1], true, true]);
});

els['timer-select-pill'] = {};
floatingOpen = true;
check('T and Shift+T are ignored while a dialog is open', [timerShortcut(false), timerShortcut(true), calls], [false, false, []]);
floatingOpen = false;
check('T toggles the timer panel', [timerShortcut(false), calls], [true, ['toggle']]);
check('Shift+T picks the timer task', [timerShortcut(true), calls], [true, ['toggle', 'select']]);
delete els['timer-select-pill'];
check('Shift+T does nothing without the pick pill', [timerShortcut(true), calls.length], [false, 2]);
calls.length = 0;
els['timer-popup'] = { hidden: true };
check('m does nothing while the timer panel is closed', [press('m').handled, calls], [false, []]);
els['timer-popup'].hidden = false;
check('t then m switches Pomodoro and Stopwatch', [press('m').handled, calls], [true, ['mode']]);

calls.length = 0;
floatingOpen = true;
check('keys are ignored while a dialog is open', [press('f').handled, press('s').handled, calls], [false, false, []]);
floatingOpen = false;
delete els['help-dialog'];
check('h does nothing on a page without the help dialog', press('h').handled, false);
els['boards-dropdown'] = { hidden: true };
els['boards-dropdown-btn'] = { click: function () { els['boards-dropdown'].hidden = false; calls.push('open'); } };
els['bs-search'] = { focus: function () { calls.push('search'); } };
calls.length = 0;
check('b opens the board list and focuses its search', [press('b').handled, calls], [true, ['open', 'search']]);
calls.length = 0;
press('b');
check('b with the list open only focuses the search', calls, ['search']);

var sub = {
  hidden: false,
  items: [0, 1, 2].map(function (i) { return { focus: function () { document.activeElement = this; focused.push(i); } }; }),
  querySelectorAll: function () { return this.items; }
};
els['reports-submenu'] = sub;
function arrow(k) { return reportsMenuKey({ key: k }); }
check('ArrowDown with nothing focused reaches the first report', [arrow('ArrowDown'), focused], [true, [0]]);
arrow('ArrowDown');
arrow('ArrowDown');
arrow('ArrowDown');
check('ArrowDown wraps past the last report', focused, [0, 1, 2, 0]);
arrow('ArrowUp');
check('ArrowUp wraps before the first report', focused[focused.length - 1], 2);
check('other keys are left to the menu', arrow('Enter'), false);
sub.hidden = true;
check('arrows are left alone while the reports menu is closed', arrow('ArrowDown'), false);

if (failures) process.exit(1);
console.log('ok');
