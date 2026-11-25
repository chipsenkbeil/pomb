const fs = require('fs');
const src = fs.readFileSync('static/pages.js', 'utf8');
const start = src.indexOf("  pages['board-settings-numbering'] = function () {");
const end = src.indexOf('\n  };\n', start) + '\n  };\n'.length;
if (start < 0 || end < start) throw new Error('board-settings-numbering page not found');

const pages = {};
const listeners = {};
const elements = {};
const element = (id) => {
  if (!elements[id]) {
    elements[id] = {
      id,
      value: '',
      hidden: true,
      textContent: '',
      focus: () => {},
      addEventListener: (type, fn) => { listeners[`${id}:${type}`] = fn; },
    };
  }
  return elements[id];
};
const document = {
  querySelector: () => ({ dataset: { boardId: 'b1' } }),
  getElementById: element,
};
const window = { location: { reload: () => {} } };
const sent = [];
global.fetch = (url, options) => {
  sent.push({ url, options });
  return new Promise(() => {});
};
eval(src.slice(start, end));
pages['board-settings-numbering']();

const prefix = element('numbering-wizard-prefix');
const startField = element('numbering-wizard-start');
const stepName = element('numbering-step-name');
const error = element('numbering-wizard-error');
const click = (id) => listeners[`${id}:click`]();
const open = (prefixValue, startValue) => {
  prefix.value = prefixValue;
  startField.value = startValue;
  click('numbering-activate');
};

let failures = 0;
const check = (name, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : `: got ${JSON.stringify(got)}`}`);
};

const prefixMessage = 'the numbering prefix must be 1 to 10 letters or digits';
const startMessage = 'the start value must be a whole number from 1';
for (const bad of ['', '   ', 'AB-C', 'ABCDEFGHIJK']) {
  open(bad, '1');
  click('numbering-wizard-next');
  check(`numbering_wizard_should_refuse_a_bad_prefix_on_step_one: '${bad}' stays on step one`, stepName.hidden, true);
  check(`numbering_wizard_should_refuse_a_bad_prefix_on_step_one: '${bad}' shows the prefix error`, [error.hidden, error.textContent], [false, prefixMessage]);
}

for (const bad of ['0', '00', '1.5', '-3', 'x']) {
  open('TASK', bad);
  click('numbering-wizard-next');
  check(`numbering_wizard_should_refuse_a_zero_start_on_step_one: '${bad}' stays on step one`, stepName.hidden, true);
  check(`numbering_wizard_should_refuse_a_zero_start_on_step_one: '${bad}' shows the start error`, [error.hidden, error.textContent], [false, startMessage]);
}

open(' TASK ', '');
click('numbering-wizard-next');
check('numbering_wizard_should_refuse_a_zero_start_on_step_one: an empty start moves on', stepName.hidden, false);
element('numbering-wizard-name').value = 'Tasks';
click('numbering-wizard-next');
check('numbering_wizard_should_refuse_a_zero_start_on_step_one: an empty start is sent as 1 with the trimmed prefix', JSON.parse(sent[0].options.body), { numbering: { name: 'Tasks', prefix: 'TASK', start: 1 } });

open('TASK', '7');
click('numbering-wizard-next');
click('numbering-wizard-next');
check('a filled start is sent as parsed', JSON.parse(sent[1].options.body).numbering.start, 7);

process.exit(failures ? 1 : 0);
