const fs = require('fs');
const src = fs.readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
const start = src.indexOf('  function wireTokenCreatedDialog(');
const end = src.indexOf('\n  }\n', start) + '\n  }\n'.length;
if (start < 0 || end < start) throw new Error('wireTokenCreatedDialog not found');

const { check, finish } = require('./support/check.js');

const elements = {};
let focused = null;
let copied = [];
function element(id) {
  if (!elements[id]) {
    const el = { id: id, hidden: true, textContent: '', listeners: {} };
    el.addEventListener = (type, handler) => { el.listeners[type] = handler; };
    el.focus = () => { focused = id; };
    elements[id] = el;
  }
  return elements[id];
}
const document = { getElementById: (id) => element(id) };
const navigator = { clipboard: { writeText: (text) => { copied.push(text); } } };
eval(src.slice(start, end));

const token = 'pomb_' + 'AbCd0123'.repeat(12);
const dialog = wireTokenCreatedDialog();
dialog.open(token);
check('token-created-dialog: a create opens the overlay', elements['token-created-dialog'].hidden, false);
check('token-created-dialog: the dialog holds the whole token', elements['token-created-value'].textContent, token);
check('token-created-dialog: focus moves into the dialog', focused, 'token-copy');

elements['token-copy'].listeners.click();
check('token-created-dialog: Copy writes the whole token to the clipboard', copied, [token]);

elements['token-created-done'].listeners.click();
check('token-created-dialog: Done closes the dialog', elements['token-created-dialog'].hidden, true);
check('token-created-dialog: Done clears the token from the page', elements['token-created-value'].textContent, '');
check('token-created-dialog: Done returns focus to Create token', focused, 'token-create-submit');

dialog.open(token);
focused = null;
let stopped = false;
elements['token-created-dialog'].listeners.keydown({ key: 'Tab', stopPropagation: () => { stopped = true; } });
check('token-created-dialog: other keys leave the dialog open', elements['token-created-dialog'].hidden, false);
elements['token-created-dialog'].listeners.keydown({ key: 'Escape', stopPropagation: () => { stopped = true; } });
check('token-created-dialog: Escape closes the dialog', elements['token-created-dialog'].hidden, true);
check('token-created-dialog: Escape clears the token', elements['token-created-value'].textContent, '');
check('token-created-dialog: Escape returns focus to Create token', focused, 'token-create-submit');
check('token-created-dialog: Escape stops at the dialog', stopped, true);

finish();
