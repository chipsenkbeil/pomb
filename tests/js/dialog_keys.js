var src = require('fs').readFileSync(process.argv[2], 'utf8');
function slice(from, to) {
  var start = src.indexOf(from);
  var end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
}

var calls = [];
var dialogOpen = false;
var modalOpen = true;
var columnKeyHints = [];
var listeners = {};
var shortcuts = { hidden: true };
function button(name) { return { click: function () { calls.push(name); } }; }
var buttons = {
  '[data-tm-menu="tm-timer-menu"]': button('timer-menu'),
  '[data-tm-menu="tm-reports-menu"]': button('reports-menu'),
  '[data-tm-menu="tm-more-menu"]': button('more-menu'),
  '[data-tm-menu="tm-add-menu"]': button('add-menu'),
};
var document = {
  addEventListener: function (type, fn) { listeners[type] = fn; },
  querySelector: function (sel) {
    if (sel === '.task-modal[data-task-id]') return modalOpen ? modal : null;
    if (buttons[sel]) return buttons[sel];
    return /dlg-overlay/.test(sel) && dialogOpen ? {} : null;
  },
  getElementById: function (id) {
    if (id === 'shortcuts-dialog') return shortcuts;
    if (id === 'manual-time-overlay') return {};
    return null;
  },
};
var window = { print: function () {} };
var modal = { querySelector: function () { return null; } };
var TimerUI = {
  togglePopup: function () { calls.push('timer-popup'); },
  beginSelect: function () { calls.push('timer-select'); },
};
var ManualTime = { open: function () { calls.push('manual-time'); } };
function reportsMenuKey() { return false; }
function closeModal() { calls.push('close'); }
function clearColumnHints() {}
function pickColumnHint() {}
function handleEscape() { calls.push('escape'); }
function openReportsMenu() { calls.push('reports'); }
function modalTaskId() { return 't1'; }
function openEstimateDialog() { calls.push('estimate'); }
function openMoveDialog() { calls.push('move'); }
function deleteModalTask() { calls.push('delete'); }
function modalAction(act) { calls.push(act); }
function boardLetter(key) { calls.push('board-' + key); return true; }
eval(slice('  var SHORTCUT_DIALOGS =', '  // ---------- Task extras'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function press(key, shift) {
  calls.length = 0;
  shortcuts.hidden = true;
  listeners.keydown({
    key: key, shiftKey: !!shift, metaKey: false, ctrlKey: false, altKey: false,
    target: { closest: function () { return null; } },
    preventDefault: function () {},
  });
  return calls.slice().concat(shortcuts.hidden ? [] : ['shortcuts']);
}

var keys = [['t'], ['T', true], ['p'], ['y'], ['e'], ['v'], ['.'], ['Delete'], ['?'], ['d'], ['a']];
dialogOpen = true;
keys.forEach(function (k) {
  check((k[1] ? 'Shift+' : '') + k[0] + ' does nothing in the task modal while a dialog is open', press(k[0], k[1]), []);
});
check('Escape still closes the dialog', press('Escape'), ['escape']);
modalOpen = false;
check('a board key does nothing while a dialog is open', press('b'), []);

dialogOpen = false;
check('b reaches the board once the dialog closes', press('b'), ['board-b']);
modalOpen = true;
check('v opens the move dialog once the dialog closes', press('v'), ['move']);
check('Delete deletes the task once the dialog closes', press('Delete'), ['delete']);
check('? opens the shortcuts once the dialog closes', press('?'), ['shortcuts']);
check('t opens the timer menu once the dialog closes', press('t'), ['timer-menu']);

if (failures) process.exit(1);
console.log('ok');
