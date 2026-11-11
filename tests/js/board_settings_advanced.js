const fs = require('fs');
const src = fs.readFileSync('static/pages.js', 'utf8');
const start = src.indexOf("  pages['board-settings-advanced'] = function () {");
const end = src.indexOf('\n  };\n', start) + '\n  };\n'.length;
if (start < 0 || end < start) throw new Error('board-settings-advanced page not found');

const pages = {};
const listeners = {};
const elements = {};
const element = (id) => {
  if (!elements[id]) {
    elements[id] = {
      id,
      checked: false,
      disabled: false,
      hidden: true,
      textContent: '',
      addEventListener: (type, fn) => { listeners[`${id}:${type}`] = fn; },
    };
  }
  return elements[id];
};
const document = {
  querySelector: () => ({ dataset: { boardId: 'b 1' } }),
  getElementById: element,
};
const sent = [];
const waiting = [];
global.fetch = (url, options) => {
  sent.push({ url, options });
  return new Promise((resolve) => {
    waiting.push((ok) => resolve({ ok, status: ok ? 200 : 500 }));
  });
};
eval(src.slice(start, end));
pages['board-settings-advanced']();

const box = element('bin-permanent-delete');
const error = element('bin-permanent-delete-error');
const click = () => {
  if (box.disabled) return;
  box.checked = !box.checked;
  listeners['bin-permanent-delete:change']();
};
const answer = (ok) => waiting.shift()(ok);
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

let failures = 0;
const check = (name, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : `: got ${JSON.stringify(got)}`}`);
};

click();
check('checkbox_should_be_disabled_while_saving: the box is disabled while the PUT is in flight', box.disabled, true);
click();
check('checkbox_should_be_disabled_while_saving: a second toggle during the save sends no PUT', sent.length, 1);
check('the PUT goes to the board config', sent[0].url, '/api/boards/b%201/config');
check('the PUT carries the new value', JSON.parse(sent[0].options.body), { bin_permanent_delete: true });

answer(true);
settle().then(() => {
  check('success: the box is enabled again', box.disabled, false);
  check('success: the box keeps the saved value', box.checked, true);
  check('success: no error is shown', error.hidden, true);
  click();
  check('success: the next change sends the next PUT', sent.length, 2);
  check('success: the next PUT carries the value the box shows', JSON.parse(sent[1].options.body), { bin_permanent_delete: false });
  check('success: the box is disabled while the second PUT is in flight', box.disabled, true);
  answer(false);
  return settle();
}).then(() => {
  check('failure: the box is enabled again', box.disabled, false);
  check('failure: the box is restored to the last saved value', box.checked, true);
  check('failure: the error text is shown', error.textContent, 'Could not save the setting.');
  check('failure: the error is unhidden', error.hidden, false);
  process.exit(failures ? 1 : 0);
});
