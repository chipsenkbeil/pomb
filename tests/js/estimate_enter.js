const src = require('fs').readFileSync(process.argv[2], 'utf8');
const fn = (start) => {
  const from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
};

let adds = 0;
const doAddEstimate = () => { adds++; };
const doMoveTask = () => {};
const doCopyTask = () => {};
const initRelationDialog = () => {};
let taskDialogsInitialized = false;

const listeners = {};
const element = (id) => ({
  hidden: false,
  classList: { contains: () => false },
  addEventListener: (type, handler) => {
    listeners[id + ':' + type] = (listeners[id + ':' + type] || []).concat(handler);
  },
});
const ids = ['mt-move-btn', 'cp-copy-btn', 'cp-name', 'move-task-dialog', 'est-add', 'est-input', 'estimate-dialog', 'duedate-dialog'];
const elements = {};
ids.forEach((id) => { elements[id] = element(id); });
const document = { getElementById: (id) => elements[id] || null };

const { initTaskDialogs } = eval('(() => {' + fn('  function initTaskDialogs() {') + 'return { initTaskDialogs };})()');

let failures = 0;
const check = (name, got, want) => {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
};
const press = (init) => {
  let prevented = false;
  const e = Object.assign({ key: 'Enter', preventDefault: () => { prevented = true; } }, init);
  (listeners['est-input:keydown'] || []).forEach((handler) => handler(e));
  return prevented;
};

initTaskDialogs();
check('the estimate input has one keydown handler', (listeners['est-input:keydown'] || []).length, 1);
check('Enter is consumed', press({}), true);
check('Enter saves the estimate once', adds, 1);
check('another key is not consumed', press({ key: 'a' }), false);
check('another key does not save', adds, 1);

if (failures) process.exit(1);
console.log('ok');
