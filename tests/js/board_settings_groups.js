const fs = require('fs');
const src = fs.readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
const start = src.indexOf("  pages['board-settings-groups'] = () => {");
const stop = src.indexOf('\n  };\n', start);
if (start < 0 || stop < start) throw new Error('board-settings-groups handler not found');
const handlerSrc = src.slice(start, stop + '\n  };\n'.length);

const pages = {};
let submitListener = null;
let clickListener = null;
const section = {
  dataset: { boardId: 'b1' },
  addEventListener: (type, handler) => { if (type === 'click') clickListener = handler; },
};
const errorBox = { hidden: true, textContent: '' };
const boxes = [{ value: 'c1' }, { value: 'c2' }];
const form = {
  elements: { limit: { value: '3' } },
  querySelectorAll: () => boxes,
  addEventListener: (type, handler) => { if (type === 'submit') submitListener = handler; },
};
const elementsById = {
  'settings-limit-groups': section,
  'settings-group-error': errorBox,
  'settings-group-form': form,
};
const document = { getElementById: (id) => elementsById[id] };
const window = { location: { reload: () => {} } };
const sent = [];
const fetch = (url, options) => {
  sent.push({ url, options });
  return Promise.resolve({ ok: true });
};
eval(handlerSrc);
pages['board-settings-groups']();

let failures = 0;
function check(name, got, want) {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  const ok = g === w;
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + g + ' want ' + w));
}

let prevented = 0;
submitListener({ preventDefault: () => { prevented++; }, target: form });
check('the create submit stops the browser submitting the form', prevented, 1);
check('the create submit sends one request', sent.length, 1);
check('the create submit posts to the board limit-groups url', sent[0].url, '/api/boards/b1/limit-groups');
check('the create submit uses POST', sent[0].options.method, 'POST');
check('the create submit sends the checked columns and the limit',
  JSON.parse(sent[0].options.body), { column_ids: ['c1', 'c2'], limit: 3 });

const row = { dataset: { groupId: 'g1' }, querySelector: () => ({ value: '5' }) };
const saveButton = {};
const saveTarget = {
  closest: (selector) => {
    if (selector === 'tr[data-group-id]') return row;
    if (selector === '[data-group-save]') return saveButton;
    return null;
  },
};
clickListener({ target: saveTarget });
check('the edit save sends one more request', sent.length, 2);
check('the edit save patches the group url', sent[1].url, '/api/boards/b1/limit-groups/g1');
check('the edit save uses PATCH', sent[1].options.method, 'PATCH');
check('the edit save sends only the new limit', JSON.parse(sent[1].options.body), { limit: 5 });

process.exit(failures ? 1 : 0);
