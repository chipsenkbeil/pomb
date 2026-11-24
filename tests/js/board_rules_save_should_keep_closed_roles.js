const source = require('fs').readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
const start = source.indexOf("  pages['board-settings-rules']");
const stop = source.indexOf("\n  pages['board-settings-labels']");
if (start < 0 || stop < start) throw new Error('column rules page not found');

const template = require('fs').readFileSync(process.argv[2].replace(/static\/app\.js$/, 'templates/board_settings.html'), 'utf8');
if (!template.includes('id="br-form"') || !template.includes('data-closed-roles="{{ rules_closed_roles }}"') || !template.includes('data-label-moves="{{ rules_label_moves }}"')) {
  throw new Error('the rules form does not carry the closed roles and the label moves');
}

const node = (props) => {
  const self = { dataset: {}, hidden: false, ...props };
  self.addEventListener = (type, handler) => { self.listeners = { ...self.listeners, [type]: handler }; };
  self.getAttribute = () => 'b1';
  self.querySelectorAll = () => [];
  self.querySelector = () => null;
  return self;
};

const sent = [];
const labelMoves = [{ from: ['c1'], to: [], into_done: true, strip: ['needs:*'] }];
const form = node({ dataset: { closedRoles: '["r1","r2"]', labelMoves: JSON.stringify(labelMoves) } });
form.requestSubmit = () => { form.listeners.submit({ preventDefault: () => {} }); };
const elements = {
  'br-form': form,
  'br-error': node({}),
  'br-saved': node({}),
  'br-locked': node({ value: 'lock' }),
  'br-standard': node({}),
};
const document = {
  getElementById: (id) => elements[id],
  querySelector: () => node({}),
};
const window = { location: { reload: () => {} } };
const fetch = (url, options) => {
  sent.push({ url, options });
  return new Promise(() => {});
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
expect('board_rules_save_should_keep_closed_roles sends the loaded closed roles', JSON.parse(sent[0].options.body).closed_roles, ['r1', 'r2']);
elements['br-standard'].listeners.click();
expect('board_rules_save_should_keep_closed_roles keeps them on a restore', JSON.parse(sent[1].options.body).closed_roles, ['r1', 'r2']);
expect('board_rules_save_should_keep_closed_roles sends the stored label moves', JSON.parse(sent[0].options.body).label_moves, labelMoves);
expect('board_rules_save_should_keep_closed_roles keeps the label moves on a restore', JSON.parse(sent[1].options.body).label_moves, labelMoves);

process.exit(failures ? 1 : 0);
