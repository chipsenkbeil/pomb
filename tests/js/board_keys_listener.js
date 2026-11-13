const src = require('fs').readFileSync(process.argv[2], 'utf8');
const slice = (from, to) => {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
}

const listeners = [];
const clicks = [];
const calls = [];
const els = {};
const addButtons = [];

const makeAdd = (columnId) => {
  const header = { children: [], appendChild(c) { this.children.push(c); c.parent = this; } };
  return {
    columnId: columnId,
    header: header,
    click: () => { clicks.push(columnId); },
    closest: () => header,
  };
}
for (let c = 1; c <= 3; c++) addButtons.push(makeAdd('col' + c));

const document = {
  addEventListener: (type, fn, capture) => { listeners.push({ type: type, fn: fn, capture: !!capture }); },
  createElement: () => {
    return { className: '', textContent: '', remove() { this.parent.children.splice(this.parent.children.indexOf(this), 1); } };
  },
  querySelectorAll: (sel) => /columnHeader-add/.test(sel) ? addButtons : [],
  querySelector: () => null,
  getElementById: (id) => els[id] || null,
  activeElement: null,
};
const BoardChrome = eval('({' + slice('    showAllMembers: function () {', '    openReports: function') + '})');
const TimerUI = eval('({' + slice('    selecting: false,', '    // Point the timer at a task') + '})');
TimerUI.closePopup = () => { calls.push('close'); };
TimerUI.selectTask = (id) => { calls.push('select:' + id); };
eval(slice('  var columnKeyHints = [];', '  // ---------- Task extras'));
const self = TimerUI;
eval(slice("      document.addEventListener('click', function (e) {\n        if (!self.selecting)", '      // KF-010'));

const listener = (type, capture) => {
  const found = listeners.filter((l) => l.type === type && l.capture === capture);
  if (!found.length) { console.log('FAIL no ' + type + ' listener'); process.exit(1); }
  return found[found.length - 1].fn;
}
const keydown = listener('keydown', false);
const click = listener('click', true);

let failures = 0;
const check = (name, got, want) => {
  const g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
const press = (k, shift) => {
  const event = { key: k, shiftKey: !!shift, prevented: false, target: null, preventDefault() { this.prevented = true; } };
  keydown(event);
  return event.prevented;
}
const hintTexts = () => {
  return addButtons.map((a) => a.header.children.map((h) => h.textContent))
    .filter((t) => t.length).map((t) => t[0]);
}

check('a shows the column hints', (press('a'), hintTexts()), ['1', '2', '3']);
press('Shift', true);
check('Shift alone keeps the hints', hintTexts(), ['1', '2', '3']);
const modifierHints = ['Control', 'Alt', 'Meta'].map((k) => (press(k), hintTexts().length));
check('a lone Control, Alt or Meta keeps the hints', modifierHints, [3, 3, 3]);
press('x');
check('a non-digit key clears the hints', hintTexts(), []);

press('a');
press('2');
check('a digit opens the add dialog for that column', [clicks, hintTexts()], [['col2'], []]);

clicks.length = 0;
press('a');
press('7');
check('a digit past the last column opens nothing', [clicks, hintTexts()], [[], []]);

const picked = { dataset: { taskId: 'task-9' } };
const event = {
  stopped: false,
  prevented: false,
  target: { closest: (sel) => sel === '.task-card' ? picked : null },
  preventDefault() { this.prevented = true; },
  stopPropagation() { this.stopped = true; },
};
click(event);
check('a click outside select mode is left alone', [calls, event.prevented, event.stopped], [[], false, false]);

els['timer-select-pill'] = { hidden: true };
calls.length = 0;
press('T', true);
check('Shift+T enters select mode', [TimerUI.selecting, els['timer-select-pill'].hidden, calls], [true, false, ['close']]);

calls.length = 0;
const miss = { target: { closest: () => null }, preventDefault() { throw new Error('prevented'); }, stopPropagation() {} };
click(miss);
check('a click off a card picks nothing', [TimerUI.selecting, calls], [true, []]);

click(event);
check('the click on a card picks it and stops the event', [calls, event.prevented, event.stopped], [['select:task-9'], true, true]);

const select = { value: '', events: [], dispatchEvent(e) { this.events.push(e.type); } };
els['f-user'] = select;
select.value = 'u1';
check('U prevents the default', press('u'), true);
check('U sets the member select to all and dispatches change', [select.value, select.events], ['all', ['change']]);

if (failures) process.exit(1);
console.log('ok');
