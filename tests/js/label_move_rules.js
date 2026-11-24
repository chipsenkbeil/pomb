const source = require('fs').readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
const start = source.indexOf("  pages['board-settings-rules']");
const stop = source.indexOf("\n  pages['board-settings-labels']");
if (start < 0 || stop < start) throw new Error('column rules page not found');

const template = require('fs').readFileSync(process.argv[2].replace(/static\/app\.js$/, 'templates/board_settings.html'), 'utf8');
const rowTemplate = require('fs').readFileSync(process.argv[2].replace(/static\/app\.js$/, 'templates/_label_move_row.html'), 'utf8');
['br-lm-rows', 'br-lm-blank', 'br-lm-add'].forEach((id) => {
  if (!template.includes('id="' + id + '"')) throw new Error('no ' + id + ' in the template');
});
['br-lm-from', 'br-lm-to', 'br-lm-done', 'br-lm-strip', 'br-lm-remove'].forEach((name) => {
  if (!rowTemplate.includes(name)) throw new Error('no ' + name + ' in the row');
});

const node = (classes, props, children) => {
  const self = { classes, parent: null, children: children || [], listeners: {}, dataset: {}, hidden: false, ...props };
  self.children.forEach((child) => { child.parent = self; });
  self.addEventListener = (type, handler) => { self.listeners[type] = handler; };
  self.getAttribute = () => 'b1';
  self.has = (cls) => self.classes.split(' ').includes(cls);
  self.closest = (selector) => {
    for (let at = self; at; at = at.parent) {
      if (selector.startsWith('#') ? at.id === selector.slice(1) : at.has(selector.slice(1))) return at;
    }
    return null;
  };
  self.remove = () => { self.parent.children.splice(self.parent.children.indexOf(self), 1); };
  self.appendChild = (child) => { child.parent = self; self.children.push(child); };
  self.cloneNode = () => makeRow([], [], false, '');
  self.querySelectorAll = (selector) => {
    let found = [];
    self.children.forEach((child) => {
      if (child.has(selector.slice(1))) found.push(child);
      found = found.concat(child.querySelectorAll(selector));
    });
    return found;
  };
  self.querySelector = (selector) => self.querySelectorAll(selector)[0] || null;
  return self;
};
const select = (cls, values) => node(cls, { selectedOptions: values.map((value) => ({ value })) });
const makeRow = (from, to, done, strip) => node('br-label-move', {}, [
  select('br-lm-from', from),
  select('br-lm-to', to),
  node('br-lm-done', { checked: done }),
  node('br-lm-strip', { value: strip }),
  node('br-lm-remove'),
]);

const first = makeRow(['c1', 'c2'], [], true, ' needs:* \n\nhold');
const second = makeRow([], ['c3'], false, '');
const rows = node('', { id: 'br-lm-rows' }, [first, second]);
const blank = node('', { content: { firstElementChild: makeRow([], [], false, '') } });
const addButton = node('', { id: 'br-lm-add' });
const form = node('', { dataset: { closedRoles: '[]' } }, [rows]);
form.requestSubmit = () => {};
const elements = {
  'br-form': form,
  'br-error': node('', {}),
  'br-saved': node('', {}),
  'br-locked': node('', { value: '' }),
  'br-standard': node('', {}),
  'br-lm-rows': rows,
  'br-lm-blank': blank,
};
const sent = [];
const answer = { ok: false, status: 400, text: () => Promise.resolve('{"message":"label_moves strip needs a label"}') };
const document = {
  getElementById: (id) => elements[id],
  querySelector: () => node('', {}),
};
const window = { location: { reload: () => {} } };
const fetch = (url, options) => {
  sent.push({ url, options });
  return Promise.resolve(answer);
};
const pages = {};
eval(source.slice(start, stop));
pages['board-settings-rules']();

let failures = 0;
const expect = (name, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got) + ' want ' + JSON.stringify(want)));
};

form.listeners.submit({ preventDefault: () => {} });
expect('label_move_rows sends one request', sent.length, 1);
expect('label_move_rows reads each row into label_moves', JSON.parse(sent[0].options.body).label_moves, [
  { from: ['c1', 'c2'], to: [], into_done: true, strip: ['needs:*', 'hold'] },
  { from: [], to: ['c3'], into_done: false, strip: [] },
]);
expect('label_move_rows PUTs the board rules', [sent[0].options.method, sent[0].url], ['PUT', '/api/boards/b1/rules']);

form.listeners.click({ target: addButton });
expect('label_move_rows adds a row from the blank one', rows.children.length, 3);
form.listeners.click({ target: first.children[4] });
expect('label_move_rows removes the row its button sits in', rows.children.length, 2);
expect('label_move_rows keeps the other rows', rows.children[0] === second, true);
form.listeners.submit({ preventDefault: () => {} });
expect('label_move_rows sends the rows left after an add and a remove', JSON.parse(sent[1].options.body).label_moves.length, 2);

new Promise((resolve) => setTimeout(resolve, 20)).then(() => {
  expect('label_move_rows shows the 400 text in the error box', [elements['br-error'].textContent, elements['br-error'].hidden], ['label_moves strip needs a label', false]);
  process.exit(failures ? 1 : 0);
});
