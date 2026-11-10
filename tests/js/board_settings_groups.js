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
const boxes = [
  { value: 'c1', checked: true },
  { value: 'c2', checked: false },
  { value: 'c3', checked: true },
];
const form = {
  elements: { limit: { value: '3' } },
  querySelectorAll: (selector) => {
    if (selector !== 'input[name="column"]:checked') throw new Error('unexpected selector ' + selector);
    return boxes.filter((box) => box.checked);
  },
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
check('skips_unchecked_columns: the create submit sends only the checked columns and the limit',
  JSON.parse(sent[0].options.body), { column_ids: ['c1', 'c3'], limit: 3 });

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

const unlinkButton = {};
const unlinkTarget = {
  closest: (selector) => {
    if (selector === 'tr[data-group-id]') return row;
    if (selector === '[data-group-unlink]') return unlinkButton;
    return null;
  },
};
clickListener({ target: unlinkTarget });
check('unlink_should_send_delete_to_the_group_url: the unlink click sends one more request', sent.length, 3);
check('unlink_should_send_delete_to_the_group_url: the unlink click deletes the group url', sent[2].url, '/api/boards/b1/limit-groups/g1');
check('unlink_should_send_delete_to_the_group_url: the unlink click uses DELETE', sent[2].options.method, 'DELETE');
check('unlink_should_send_delete_to_the_group_url: the unlink click sends no body', sent[2].options.body, undefined);

const lowRow = { dataset: { groupId: 'g1' }, querySelector: () => ({ value: '0' }) };
const lowSaveTarget = {
  closest: (selector) => {
    if (selector === 'tr[data-group-id]') return lowRow;
    if (selector === '[data-group-save]') return saveButton;
    return null;
  },
};
clickListener({ target: lowSaveTarget });
check('save_should_reject_a_limit_below_one: the save of 0 sends no request', sent.length, 3);
check('save_should_reject_a_limit_below_one: the error box is shown', errorBox.hidden, false);
check('save_should_reject_a_limit_below_one: the error box names the minimum', errorBox.textContent, 'The limit must be at least 1.');

process.exit(failures ? 1 : 0);
