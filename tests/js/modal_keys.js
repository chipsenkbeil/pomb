var src = require('fs').readFileSync(process.argv[2], 'utf8');
function slice(from, to) {
  var start = src.indexOf(from);
  var end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
}

var actions = [];
var clicks = [];
var focused = [];
var dialogOpen = false;
var nameInput = { focus: function () { focused.push('name'); }, select: function () { focused.push('select'); } };
var addButton = { click: function () { clicks.push('add'); } };
var sources = {};
var panels = ['board', 'task'].map(function (name) {
  return { name: name, hidden: name !== 'board', getAttribute: function () { return name; } };
});
var dialog = { querySelectorAll: function () { return panels; } };
var listeners = {};
var printed = 0;
var window = { print: function () { printed++; } };
var document = {
  addEventListener: function (type, fn) { listeners[type] = fn; },
  querySelector: function (sel) {
    if (sel === '[data-tm-menu="tm-add-menu"]') return addButton;
    return /dlg-overlay/.test(sel) && dialogOpen ? {} : null;
  },
  getElementById: function (id) { return id === 'modal-name' ? nameInput : id === 'shortcuts-dialog' ? dialog : null; }
};
function modalAction(act, source) { actions.push([act, source]); }
var modal = {
  querySelector: function (sel) {
    var m = /data-tm-act="([^"]*)"/.exec(sel);
    return m ? (sources[m[1]] = sources[m[1]] || { act: m[1] }) : null;
  }
};
eval(slice('  var MODAL_LETTER_ACTS = {', '  document.addEventListener(\'keydown\''));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

var wanted = {
  c: 'pick-color', d: 'add-description', f: 'add-attachment', k: 'add-comment', l: 'add-label',
  m: 'add-member', r: 'add-relation', s: 'add-subtask', u: 'add-duedate'
};
Object.keys(wanted).forEach(function (key) {
  actions.length = 0;
  check(key + ' is handled', modalLetter(modal, key), true);
  check(key + ' runs ' + wanted[key] + ' from its control', actions, [[wanted[key], { act: wanted[key] }]]);
});

check('a opens the Add menu', [modalLetter(modal, 'a'), clicks], [true, ['add']]);
check('n focuses and selects the name', [modalLetter(modal, 'n'), focused], [true, ['name', 'select']]);
check('an unmapped letter is not handled', modalLetter(modal, 'z'), false);

actions.length = 0;
dialogOpen = true;
check('letters are ignored while a dialog is open', [modalLetter(modal, 'd'), modalLetter(modal, 'a'), modalLetter(modal, 'n'), actions], [false, false, false, []]);
dialogOpen = false;

listeners.change({ target: { id: 'other', value: 'task' } });
check('another select leaves the shortcut lists alone', panels.map(function (p) { return p.hidden; }), [false, true]);
listeners.change({ target: { id: 'shortcuts-tab', value: 'task' } });
check('the select shows the Task details list', panels.map(function (p) { return p.hidden; }), [true, false]);
listeners.change({ target: { id: 'shortcuts-tab', value: 'board' } });
check('the select shows the Board list', panels.map(function (p) { return p.hidden; }), [false, true]);

listeners.click({ target: { closest: function () { return null; } } });
check('other clicks do not print', printed, 0);
listeners.click({ target: { closest: function (sel) { return sel === '#shortcuts-print' ? {} : null; } } });
check('the Print list button prints', printed, 1);

if (failures) process.exit(1);
console.log('ok');
