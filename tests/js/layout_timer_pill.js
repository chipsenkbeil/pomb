const src = require('fs').readFileSync(process.argv[2], 'utf8');
const slice = (from, to) => {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
};

const board = { hidden: false };
const view = { hidden: true };
const pill = { hidden: false };
const elements = { 'layout-view': view, 'timer-pill': pill };
const document = {
  querySelector: (sel) => (sel === 'main.board-wrap' ? board : null),
  getElementById: (id) => elements[id] || null,
};
const window = { location: { pathname: '/board', search: '' }, history: { replaceState: () => {} } };
const initLayoutSortables = () => {};
const layout = eval('({' + slice('    // layout-edit view', '    peopleDialog: function () {') + '})');

const { check, finish } = require('./support/check.js');

layout.editLayout();
check('editLayout keeps the timer pill visible', pill.hidden, false);
check('editLayout hides the board', board.hidden, true);
check('editLayout shows the layout view', view.hidden, false);

layout.exitLayout();
check('exitLayout keeps the timer pill visible', pill.hidden, false);
check('exitLayout shows the board again', board.hidden, false);
check('exitLayout hides the layout view', view.hidden, true);

finish();
