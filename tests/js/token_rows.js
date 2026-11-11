const fs = require('fs');
const src = fs.readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
const formStart = src.indexOf('  var TokenForm = {');
const formEnd = src.indexOf('\n  };\n', formStart) + '\n  };\n'.length;
const start = src.indexOf('  function tokenRequest(');
const stop = src.indexOf("\n  pages['board-settings-webhooks']");
if (formStart < 0 || formEnd < formStart) throw new Error('TokenForm not found');
if (start < 0 || stop < start) throw new Error('token row helpers not found');
eval(src.slice(formStart, formEnd));
eval(src.slice(start, stop));

let failures = 0;
function check(name, got, want) {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got) + ' want ' + JSON.stringify(want)));
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));
const reply = (status, body) => Promise.resolve({
  ok: status < 400,
  status: status,
  text: () => Promise.resolve(body === undefined ? '' : JSON.stringify(body)),
});

const elements = {};
let rows = [];
let sent = [];
let reloads = 0;
let respond = () => new Promise(() => {});

function element(id) {
  if (!elements[id]) {
    const el = { id: id, hidden: false, value: '', textContent: '', listeners: {}, children: [] };
    el.addEventListener = (type, handler) => { el.listeners[type] = handler; };
    el.appendChild = (child) => { el.children.push(child); };
    el.focus = () => {};
    elements[id] = el;
  }
  return elements[id];
}

function button() {
  const btn = { listeners: {} };
  btn.addEventListener = (type, handler) => { btn.listeners[type] = handler; };
  btn.click = () => btn.listeners.click({ preventDefault: () => {} });
  return btn;
}

function tokenRow(id, name) {
  const buttons = { '.tk-show': button(), '.tk-edit': button(), '.tk-delete': button() };
  return {
    dataset: { tokenId: id, name: name },
    querySelector: (selector) => buttons[selector],
  };
}

const document = {
  getElementById: (id) => element(id),
  querySelectorAll: () => rows,
  createElement: (tag) => ({ tagName: tag, textContent: '' }),
};
const fetch = (url, options) => {
  sent.push({ url: url, options: options });
  return respond(url, options);
};
const window = {
  pombPasskey: {
    fetch: (url, options) => {
      sent.push({ url: url, options: options });
      return new Promise(() => {});
    },
  },
  location: { reload: () => { reloads += 1; } },
};

function reset() {
  Object.keys(elements).forEach((id) => { delete elements[id]; });
  rows = [];
  sent = [];
  reloads = 0;
  respond = () => new Promise(() => {});
  element('token-table');
  element('tk-error').hidden = true;
  ['tk-show-overlay', 'tk-edit-overlay', 'tk-delete-overlay'].forEach((id) => { element(id).hidden = true; });
}

(async () => {
  reset();
  rows = [tokenRow('tok_1', 'CI bot')];
  respond = () => reply(200, {
    name: 'CI bot',
    prefix: 'pomb_ab',
    last4: 'Wx9z',
    expires_at: 1767225600,
    boards: [{ name: 'Alpha' }, { name: 'Beta' }],
    permissions: ['tasks.read'],
  });
  wireTokenRows();
  rows[0].querySelector('.tk-show').click();
  await settle();
  check('show_token_should_fill_the_dialog: the click sends one GET for the row id',
    sent.map((s) => [s.url, s.options.method]), [['/api/v1/auth/tokens/tok_1', 'GET']]);
  check('show_token_should_fill_the_dialog: the name fills the dialog',
    elements['tk-show-name'].textContent, 'CI bot');
  check('show_token_should_fill_the_dialog: the expiry fills the dialog',
    elements['tk-show-expiry'].textContent, 'Expires 2026-01-01');
  check('show_token_should_fill_the_dialog: the named boards fill the dialog',
    elements['tk-show-boards'].textContent, 'Boards: Alpha, Beta');
  check('show_token_should_fill_the_dialog: the masked value fills the dialog',
    elements['tk-show-value'].textContent, 'pomb_ab…Wx9z');
  check('show_token_should_fill_the_dialog: the overlay opens',
    elements['tk-show-overlay'].hidden, false);

  respond = () => reply(200, {
    name: 'Deploy',
    prefix: 'pomb_cd',
    last4: 'Q1r2',
    expires_at: null,
    boards: 'all',
    permissions: [],
  });
  rows[0].querySelector('.tk-show').click();
  await settle();
  check('show_token_should_fill_the_dialog: an all-boards token reads Boards: all',
    elements['tk-show-boards'].textContent, 'Boards: all');
  check('show_token_should_fill_the_dialog: a token without expiry reads Never expires',
    elements['tk-show-expiry'].textContent, 'Never expires');

  reset();
  rows = [tokenRow('tok_2', 'Old laptop')];
  wireTokenRows();
  rows[0].querySelector('.tk-delete').click();
  check('delete_confirm_should_send_delete_for_the_row: the click opens the overlay',
    elements['tk-delete-overlay'].hidden, false);
  check('delete_confirm_should_send_delete_for_the_row: the overlay names the row',
    elements['tk-delete-name'].textContent, 'Old laptop');
  check('delete_confirm_should_send_delete_for_the_row: opening sends nothing',
    sent.length, 0);

  respond = () => reply(200);
  elements['tk-delete-ok'].listeners.click({ preventDefault: () => {} });
  await settle();
  check('delete_confirm_should_send_delete_for_the_row: the confirm sends one DELETE for the row id',
    sent.map((s) => [s.url, s.options.method]), [['/api/v1/auth/tokens/tok_2', 'DELETE']]);
  check('delete_confirm_should_send_delete_for_the_row: the confirm closes the overlay',
    elements['tk-delete-overlay'].hidden, true);
  check('delete_confirm_should_send_delete_for_the_row: a successful delete reloads the page',
    reloads, 1);

  reset();
  rows = [tokenRow('tok_3', 'Stale')];
  respond = () => reply(404, { message: 'token not found' });
  wireTokenRows();
  rows[0].querySelector('.tk-show').click();
  await settle();
  check('show_failure_writes_error: the request was sent once',
    sent.length, 1);
  check('show_failure_writes_error: the message lands in tk-error',
    elements['tk-error'].textContent, 'token not found');
  check('show_failure_writes_error: tk-error is shown',
    elements['tk-error'].hidden, false);
  check('show_failure_writes_error: the overlay stays closed',
    elements['tk-show-overlay'].hidden, true);

  process.exit(failures ? 1 : 0);
})().catch((error) => {
  console.error(error);
  process.exit(1);
});
