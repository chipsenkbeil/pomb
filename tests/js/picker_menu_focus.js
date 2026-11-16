const fs = require('fs');
const src = fs.readFileSync('static/pages.js', 'utf8');
const from = src.indexOf('  var pickerMenu = null;');
const resize = "  window.addEventListener('resize', closePickerMenu);\n";
const to = src.indexOf(resize, from);
if (from < 0 || to < from) throw new Error('picker menu not found');

const handlers = {};
const document = {
  body: { appendChild(node) { return node; } },
  addEventListener(type, fn) { handlers[type] = fn; },
};
const window = { innerHeight: 800, innerWidth: 1000, addEventListener() {} };
eval(src.slice(from, to + resize.length));

let failures = 0;
const check = (name, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got)));
};

let focused = null;
const option = (name) => ({ name, focus() { focused = this; }, contains(t) { return t === this; } });
const trigger = () => ({
  dataset: { pickKey: 'access' },
  focus() { focused = this; },
  contains(t) { return t === this; },
  getBoundingClientRect() { return { top: 100, bottom: 120, left: 0 }; },
  setAttribute() {},
});
const openMenu = (items) => {
  const anchor = trigger();
  const node = {
    items,
    style: {},
    offsetHeight: 40,
    offsetWidth: 100,
    removed: false,
    contains(t) { return t === this || items.indexOf(t) >= 0; },
    querySelectorAll() { return items; },
    remove() { this.removed = true; },
  };
  focused = null;
  openPickerMenu(anchor, node);
  return { anchor, node, items };
};
const press = (target, key, shiftKey) => {
  let prevented = false;
  handlers.keydown({ key, shiftKey: !!shiftKey, target, preventDefault() { prevented = true; }, stopPropagation() {} });
  return prevented;
};

let a = openMenu([option('first'), option('middle'), option('last')]);
let prevented = press(a.anchor, 'Tab', false);
check('tab_from_trigger_focuses_first_option', [prevented, focused === a.items[0], a.node.removed], [true, true, false]);

a = openMenu([option('first'), option('middle'), option('last')]);
prevented = press(a.anchor, 'Tab', true);
check('shift_tab_from_trigger_focuses_last_option', [prevented, focused === a.items[2]], [true, true]);

a = openMenu([option('first'), option('middle'), option('last')]);
prevented = press(a.items[0], 'Tab', true);
check('shift_tab_from_first_option_returns_to_trigger_and_keeps_menu_open', [prevented, focused === a.anchor, a.node.removed, pickerMenu !== null], [true, true, false, true]);

a = openMenu([option('first'), option('middle'), option('last')]);
prevented = press(a.items[1], 'Tab', false);
check('tab_between_options_is_left_to_the_browser', [prevented, focused, a.node.removed], [false, null, false]);

a = openMenu([option('first'), option('middle'), option('last')]);
prevented = press(a.items[2], 'Tab', false);
check('tab_from_last_option_closes_menu_and_returns_to_trigger', [prevented, focused === a.anchor, a.node.removed, pickerMenu], [true, true, true, null]);

a = openMenu([option('first'), option('middle'), option('last')]);
prevented = press(a.items[1], 'Escape', false);
check('escape_closes_menu_and_returns_focus_to_trigger', [prevented, focused === a.anchor, a.node.removed, pickerMenu], [false, true, true, null]);

a = openMenu([]);
prevented = press(a.anchor, 'Tab', false);
check('tab_from_trigger_with_no_options_is_left_to_the_browser', [prevented, a.node.removed], [false, false]);

a = openMenu([option('first')]);
handlers.focusin({ target: { contains() { return false; } } });
check('focus_moving_outside_the_menu_closes_it', [a.node.removed, pickerMenu], [true, null]);

console.log('failures=' + failures);
if (failures) throw new Error(failures + ' picker menu focus check(s) failed');
