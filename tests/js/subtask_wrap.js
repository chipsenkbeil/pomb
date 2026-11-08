const path = require('path');
const fs = require('fs');
const src = fs.readFileSync(process.argv[2], 'utf8');
const css = fs.readFileSync(path.join(path.dirname(process.argv[2]), 'style.css'), 'utf8');
const slice = (from, to) => {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
};

const taskId = 't1';
let modalDirty = false;
const toasts = [];
const calls = [];
const toast = (msg) => { toasts.push(msg); };
const api = (url, method, body) => {
  calls.push([method, body && body.name]);
  return Promise.resolve({ ok: true, json: () => Promise.resolve({ name: body.name }) });
};
const created = [];
const document = {
  createElement: (tag) => {
    const el = {
      tagName: tag, style: {}, value: '', scrollHeight: 48, listeners: {},
      setAttribute: () => {},
      focus: () => {}, select: () => {},
      addEventListener: (type, fn) => { el.listeners[type] = fn; },
      replaceWith: (other) => { el.replacedBy = other; },
    };
    created.push(el);
    return el;
  },
};
const row = { dataset: { subtaskId: 's1' }, querySelector: () => null };
const makeName = (text) => ({
  textContent: text,
  closest: () => row,
  replaceWith: () => {},
});
eval(slice('    function startSubtaskEdit(nameEl) {', '\n  }\n\n  // ---------- Members dialog'));

let failures = 0;
const check = (name, got, want) => {
  const g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
};
const key = (edit, k) => {
  let prevented = false;
  edit.listeners.keydown({ key: k, preventDefault: () => { prevented = true; } });
  return prevented;
};

const long = 'A long outcome sentence that runs well past one line of the subtask list and must stay readable';

const nameA = makeName(long);
startSubtaskEdit(nameA);
const editA = created[created.length - 1];
check('the edit field is a textarea', editA.tagName, 'textarea');
check('the edit field grows to its text', editA.style.height, '48px');
editA.value = '  ' + long + ' now\nwith a pasted break  ';
check('Enter is consumed', key(editA, 'Enter'), true);
check('Enter saves the trimmed single-line text', calls, [['PATCH', long + ' now with a pasted break']]);

calls.length = 0;
const nameB = makeName(long);
startSubtaskEdit(nameB);
const editB = created[created.length - 1];
editB.value = 'changed text';
check('Escape is consumed', key(editB, 'Escape'), true);
check('Escape saves nothing', calls, []);
check('Escape restores the name element', editB.replacedBy === nameB, true);
editB.listeners.blur();
check('blur after Escape saves nothing', calls, []);

const rule = (sel) => {
  const m = css.match(new RegExp('\\n' + sel.replace('.', '\\.') + ' \\{([^}]*)\\}'));
  return m ? m[1] : '';
};
const nameRule = rule('.tm-subtask-name');
check('the subtask name has a rule', nameRule !== '', true);
check('the subtask name does not truncate', /nowrap|ellipsis/.test(nameRule), false);
check('the subtask name wraps long words', /overflow-wrap:\s*anywhere/.test(nameRule), true);
check('the subtask row aligns items to the first line', /align-items:\s*flex-start/.test(rule('.tm-subtask')), true);

Promise.resolve().then(() => {
  if (failures) process.exit(1);
  console.log('subtask_wrap ok');
});
