const src = require('fs').readFileSync(process.argv[2], 'utf8');
const start = src.indexOf('  function initBoardGeneralActions(');
const end = src.indexOf('\n  }\n', start) + '\n  }\n'.length;
if (start < 0 || end < start) throw new Error('initBoardGeneralActions not found');

const listeners = {};
const elements = {};
const element = (id) => {
  if (!elements[id]) {
    elements[id] = {
      id,
      hidden: false,
      textContent: '',
      dataset: {},
      elements: { name: { value: '', focus: () => {} } },
      addEventListener: (type, fn) => { listeners[`${id}:${type}`] = fn; },
    };
  }
  return elements[id];
};
const document = {
  querySelector: () => ({ dataset: { boardId: 'b 1' } }),
  getElementById: element,
};
const window = { location: { href: '' } };
const calls = [];
let reply = null;
const api = (path, method, data) => {
  calls.push({ path, method, data });
  return Promise.resolve(reply);
};
const initChangeBoardOwner = () => {};
let boardGeneralInitialized = false;
eval(src.slice(start, end));
initBoardGeneralActions();

let failures = 0;
const check = (name, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : `: got ${JSON.stringify(got)}`}`);
};
const submit = (name) => {
  let prevented = false;
  const form = element('board-name-form');
  form.elements.name.value = name;
  listeners['board-name-form:submit']({
    preventDefault: () => { prevented = true; },
    target: form,
  });
  return prevented;
};
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

reply = { ok: true };
const prevented = submit('Renamed');
settle().then(() => {
  check('the submit is not left to the browser', prevented, true);
  check('the name goes to the config PUT', calls[0], {
    path: '/api/boards/b%201/config',
    method: 'PUT',
    data: { name: 'Renamed' },
  });
  check('a saved name reloads the settings page', window.location.href, '/b/b%201/settings?saved=1');
  window.location.href = '';
  reply = { ok: false, status: 400, text: () => Promise.resolve('Board name must be 1-100 characters.') };
  submit('');
  return settle();
}).then(() => {
  check('a refused name stays on the page', window.location.href, '');
  check('a refused name shows the error text', element('board-action-error').textContent, 'Board name must be 1-100 characters.');
  check('a refused name unhides the error', element('board-action-error').hidden, false);
  process.exit(failures ? 1 : 0);
});
