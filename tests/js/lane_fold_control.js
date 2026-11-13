const src = require('fs').readFileSync(process.argv[2], 'utf8');
const slice = (from, to) => {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
};

const lanes = [{ id: 'lane1', name: 'Ops', collapsed: false }, { id: 'lane2', name: 'Dev', collapsed: true }];
const patches = [];
let laneMenuSwimlaneId = null;
const foldItem = { hidden: false, textContent: 'Collapse' };
const items = {};
const menu = {
  querySelector: (sel) => {
    const act = /"(.*)"/.exec(sel)[1];
    if (act === 'fold') return foldItem;
    items[act] = items[act] || { hidden: false };
    return items[act];
  },
};
const document = { getElementById: () => menu };
const laneOrder = () => lanes;
const patchLane = (id, body, failure) => { patches.push([id, body, failure]); };
const placeMenu = () => {};
const hideFloatingMenus = () => {};
const openEditLaneDialog = () => {};
eval(slice('  function laneById(', '\n  function boardColumns'));
eval(slice('  function toggleLaneFold(', '\n  function moveSwimlane'));
eval(slice('  function openLaneMenu(', '\n  function initBoardMenus'));
eval('function onFoldClick(e) {' + slice("var foldToggle = e.target.closest('[data-lane-fold-toggle]');", "if (e.target.closest('[data-lane-names-toggle]'))") + '}');
eval('function onMenuClick(e) {' + slice("var btn = e.target.closest('[data-lane-act]');", "\n    });\n\n    // ---------- card context menu") + '}');

let failures = 0;
const check = (name, got, want) => {
  const g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
};
const sent = () => patches.map((p) => p.slice(0, 2));
const chevronClick = (id) => {
  const chevron = { getAttribute: () => id };
  onFoldClick({ target: { closest: () => chevron } });
};
const menuClick = (act) => {
  const btn = { getAttribute: () => act };
  onMenuClick({ target: { closest: () => btn } });
};

chevronClick('lane1');
check('chevron on an open lane PATCHes collapsed true', sent(), [['lane1', { collapsed: true }]]);
patches.length = 0;
chevronClick('lane2');
check('chevron on a folded lane PATCHes collapsed false', sent(), [['lane2', { collapsed: false }]]);

patches.length = 0;
openLaneMenu('lane1', 'board', 0, 0);
check('menu on an open lane reads Collapse', foldItem.textContent, 'Collapse');
check('menu shows the fold item in the board', foldItem.hidden, false);
menuClick('fold');
check('menu Collapse folds the open lane', sent(), [['lane1', { collapsed: true }]]);

patches.length = 0;
openLaneMenu('lane2', 'board', 0, 0);
check('menu on a folded lane reads Expand', foldItem.textContent, 'Expand');
menuClick('fold');
check('menu Expand unfolds the folded lane', sent(), [['lane2', { collapsed: false }]]);

if (failures) process.exit(1);
console.log('ok');
