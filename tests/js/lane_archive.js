const src = require('fs').readFileSync(process.argv[2], 'utf8');
const slice = (from, to) => {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
};

const lanes = [{ id: 'lane1', name: 'Ops', count: 3 }, { id: 'lane2', name: 'Dev', count: 1 }];
const calls = [];
const toasts = [];
const confirms = [];
let reloads = 0;
let response = { ok: true, body: {} };
const window = { location: { reload: () => { reloads++; } } };
const api = (path, method, data) => {
  calls.push([path, method, data]);
  const res = {
    ok: response.ok,
    text: () => Promise.resolve(''),
    json: () => Promise.resolve(response.body),
  };
  return Promise.resolve(res);
};
const drained = (res) => res.text().then(() => res);
const toast = (message) => { toasts.push(message); };
const laneOrder = () => lanes;
const showConfirmDialog = (title, message, okLabel, onConfirm) => { confirms.push({ title, message, okLabel, onConfirm }); };

const makeElement = (tag) => ({
  tag,
  hidden: true,
  children: [],
  listeners: {},
  text: '',
  get textContent() { return this.text; },
  set textContent(value) {
    this.text = value;
    if (value === '') this.children = [];
  },
  appendChild(child) { this.children.push(child); },
  addEventListener(name, fn) { this.listeners[name] = fn; },
});
const elements = {
  'archived-lanes-list': makeElement('ul'),
  'archived-lanes-note': makeElement('p'),
  'archived-lanes-dialog': makeElement('div'),
};
let archivedData = [];
const document = {
  getElementById: (id) => elements[id],
  createElement: makeElement,
  querySelectorAll: () => archivedData,
};
let laneMenuSwimlaneId = null;
const hideFloatingMenus = () => {};
const { archiveSwimlane, openArchivedLanes } = eval('(() => {' +
  slice('  const setLaneArchived = ', '\n  function inlineLaneForm') +
  'return { archiveSwimlane, openArchivedLanes }; })()');
eval('function onMenuClick(e) {' + slice("var btn = e.target.closest('[data-lane-act]');", "\n    });\n\n    // ---------- card context menu") + '}');
const menuClick = (act) => {
  const btn = { getAttribute: () => act };
  onMenuClick({ target: { closest: () => btn } });
};

const { check, finish } = require('./support/check.js');
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

const run = async () => {
  laneMenuSwimlaneId = 'lane1';
  menuClick('archive');
  check('the lane menu Archive item asks before it PATCHes', calls, []);
  check('confirm names the lane and its card count', confirms[0].message,
    'Archive the swimlane Ops? Its 3 cards leave the board and stay frozen until the lane is unarchived.');
  check('confirm button reads Archive', confirms[0].okLabel, 'Archive');
  confirms[0].onConfirm();
  await settle();
  check('confirmed archive PATCHes archived true', calls, [['/api/swimlanes/lane1', 'PATCH', { archived: true }]]);
  check('confirmed archive reloads the board', reloads, 1);

  calls.length = 0;
  laneMenuSwimlaneId = 'lane2';
  menuClick('archive');
  check('confirm uses the singular for one card', confirms[1].message.includes('Its 1 card leave'), true);
  response = { ok: false, body: { message: 'cannot archive the last unarchived swimlane on a board' } };
  confirms[1].onConfirm();
  await settle();
  check('a refusal shows the API message', toasts, ['cannot archive the last unarchived swimlane on a board']);
  check('a refusal does not reload', reloads, 1);

  response = { ok: true, body: {} };
  calls.length = 0;
  archivedData = [
    { dataset: { laneId: 'lane3', laneName: 'Parity', taskCount: '12' } },
    { dataset: { laneId: 'lane4', laneName: 'Old', taskCount: '0' } },
  ];
  openArchivedLanes();
  const list = elements['archived-lanes-list'];
  check('dialog opens', elements['archived-lanes-dialog'].hidden, false);
  check('dialog lists each archived lane with its card count',
    list.children.map((item) => item.children[0].textContent), ['Parity (12 cards)', 'Old (0 cards)']);
  check('dialog offers Unarchive per lane', list.children.map((item) => item.children[1].textContent), ['Unarchive', 'Unarchive']);
  list.children[0].children[1].listeners.click();
  await settle();
  check('Unarchive PATCHes archived false', calls, [['/api/swimlanes/lane3', 'PATCH', { archived: false }]]);
  check('Unarchive reloads the board', reloads, 2);

  archivedData = [];
  openArchivedLanes();
  check('an empty dialog says no lane is archived', elements['archived-lanes-note'].textContent, 'No lanes are archived.');
};

run().then(() => {
  finish();
});
