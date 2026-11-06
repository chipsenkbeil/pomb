var fs = require('fs');
var src = fs.readFileSync('static/pages.js', 'utf8');
var start = src.indexOf('  var TokenForm = {');
var end = src.indexOf('\n  };\n', start) + '\n  };\n'.length;
if (start < 0 || end < start) throw new Error('TokenForm not found');
eval(src.slice(start, end));

var ALL = ['read', 'work', 'lead', 'admin'];
var UP = ['work', 'lead', 'admin'];
var LEAD = ['lead', 'admin'];
var catalog = [
  { name: 'boards.read', group: 'boards', description: 'See boards.', presets: ALL, required: true },
  { name: 'boards.create', group: 'boards', description: 'Make boards.', presets: LEAD },
  { name: 'tasks.read', group: 'tasks', description: 'Read tasks.', presets: ALL },
  { name: 'tasks.create', group: 'tasks', description: 'Create tasks.', presets: UP },
  { name: 'tasks.bin', group: 'tasks', description: 'Bin tasks.', presets: LEAD },
  { name: 'tasks.delete', group: 'tasks', description: 'Delete tasks.', presets: ['admin'] },
  { name: 'subtasks.edit', group: 'subtasks', description: 'Edit subtasks.', presets: UP },
  { name: 'subtasks.delete', group: 'subtasks', description: 'Delete subtasks.', presets: ['admin'] },
  { name: 'columns.create', group: 'columns', description: 'Make columns.', presets: LEAD },
  { name: 'settings.read', group: 'settings', description: 'Read settings.', presets: ALL },
  { name: 'settings.write', group: 'settings', description: 'Write settings.', presets: ['admin'] },
  { name: 'feeds.manage', group: 'feeds', description: 'Feeds.', presets: ['admin'] }
];
var areas = TokenForm.areas(catalog);
function area(name) { return areas.filter(function (a) { return a.name === name; })[0]; }

var failures = 0;
function check(name, got, want) {
  var ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got)));
}

check('areas keep catalog order',
  areas.map(function (a) { return a.name + ':' + a.items.length; }),
  ['boards:2', 'tasks:4', 'subtasks:2', 'columns:1', 'settings:2', 'feeds:1']);
check('read holds the read-tier permissions',
  TokenForm.levelPermissions(area('tasks'), 'read'), ['tasks.read']);
check('write holds read, work and lead',
  TokenForm.levelPermissions(area('tasks'), 'write'), ['tasks.read', 'tasks.create', 'tasks.bin']);
check('full holds every permission of the area',
  TokenForm.levelPermissions(area('tasks'), 'full'), ['tasks.read', 'tasks.create', 'tasks.bin', 'tasks.delete']);
check('an area with all three tiers offers every level and Custom',
  TokenForm.levels(area('tasks')), ['read', 'write', 'full', 'custom']);
check('an area without read-tier permissions skips Read',
  TokenForm.levels(area('subtasks')), ['write', 'full', 'custom']);
check('an area with only lead-tier permissions offers Write and Custom',
  TokenForm.levels(area('columns')), ['write', 'custom']);
check('an area with only admin-tier permissions offers Full and Custom',
  TokenForm.levels(area('feeds')), ['full', 'custom']);
check('a level equal to the one before it is not offered twice',
  TokenForm.levels(area('settings')), ['read', 'full', 'custom']);
check('boards offers Read, Write and Custom',
  TokenForm.levels(area('boards')), ['read', 'write', 'custom']);
check('a held set that is a level reads as that level',
  TokenForm.levelOf(area('tasks'), ['tasks.read', 'tasks.create', 'tasks.bin']), 'write');
check('a held set that is no level reads as Custom',
  TokenForm.levelOf(area('tasks'), ['tasks.read', 'tasks.delete']), 'custom');
check('other areas do not change the level',
  TokenForm.levelOf(area('tasks'), ['tasks.read', 'boards.create']), 'read');
check('shown lists the areas that hold a permission',
  TokenForm.shown(areas, ['tasks.read', 'feeds.manage']), ['tasks', 'feeds']);
check('the required permission is added when absent and kept once',
  [TokenForm.withRequired(catalog, ['tasks.read']), TokenForm.withRequired(catalog, ['boards.read'])],
  [['tasks.read', 'boards.read'], ['boards.read']]);
check('the level note names what a level adds',
  [TokenForm.levelNote(area('tasks'), 'read'), TokenForm.levelNote(area('tasks'), 'write'),
    TokenForm.levelNote(area('tasks'), 'full')],
  ['Includes read', 'Also create, bin', 'Also delete']);

var roles = [
  { id: 'r1', name: 'planner', permissions: ['boards.read', 'tasks.read', 'tasks.create'] },
  { id: 'r2', name: 'reviewer', permissions: ['boards.read', 'tasks.read'] },
  { id: 'r3', name: 'release', permissions: ['tasks.read', 'boards.read'] },
  { id: 'r4', name: 'solo', permissions: ['feeds.manage'] }
];
check('rows that match one role switch back to it',
  TokenForm.sync(catalog, ['tasks.create', 'tasks.read', 'boards.read'], roles).role.name, 'planner');
check('a match carries no hint',
  TokenForm.sync(catalog, ['tasks.create', 'tasks.read', 'boards.read'], roles).hint, '');
var two = TokenForm.sync(catalog, ['boards.read', 'tasks.read'], roles);
check('rows that match two roles stay Custom', two.role, null);
check('the hint names both roles', two.hint, 'These rows match the roles reviewer and release.');
var none = TokenForm.sync(catalog, ['boards.read', 'tasks.read', 'tasks.bin'], roles);
check('rows that match no role stay Custom without a hint', [none.role, none.hint], [null, '']);
check('a role missing one of the rows does not match',
  TokenForm.matches(catalog, ['boards.read'], roles).length, 0);
check('three matches are named with commas',
  TokenForm.sync(catalog, ['boards.read'], [
    { id: '1', name: 'x', permissions: ['boards.read'] },
    { id: '2', name: 'y', permissions: ['boards.read'] },
    { id: '3', name: 'z', permissions: ['boards.read'] }
  ]).hint, 'These rows match the roles x, y and z.');

check('standard roles list in their fixed order, then custom ones alphabetically',
  TokenForm.roleOrder([
    { id: '1', name: 'zed', permissions: [] }, { id: '2', name: 'lead', permissions: [] },
    { id: '3', name: 'alpha', permissions: [] }, { id: '4', name: 'planner', permissions: [] },
    { id: '5', name: 'release', permissions: [] }, { id: '6', name: 'verifier', permissions: [] },
    { id: '7', name: 'implementer', permissions: [] }, { id: '8', name: 'reviewer', permissions: [] }
  ]).map(function (role) { return role.name; }),
  ['planner', 'implementer', 'reviewer', 'verifier', 'release', 'lead', 'alpha', 'zed']);
check('a standard role has its one-line note and a custom role its count',
  [TokenForm.roleNote({ name: 'verifier', permissions: ['a', 'b'] }),
    TokenForm.roleNote({ name: 'lead', permissions: ['a'] }),
    TokenForm.roleNote({ name: 'solo', permissions: ['a', 'b'] })],
  ['Ticks outcomes', 'Plans and runs the board, issues child tokens', '2 permissions']);

var noRead = [{ id: 'n1', name: 'verifier', permissions: ['tasks.read'] }];
check('a role without the required permission matches rows that hold it',
  TokenForm.sync(catalog, ['tasks.read', 'boards.read'], noRead).role.name, 'verifier');
check('a role with a permission outside the catalog still matches',
  TokenForm.sync(catalog, ['tasks.read', 'boards.read'],
    [{ id: 'n2', name: 'old', permissions: ['tasks.read', 'gone.away'] }]).role.name, 'old');
var named = TokenForm.sync(catalog, ['boards.read', 'tasks.read'], roles, 'release');
check('a role name picks between two matching roles', [named.role.name, named.hint], ['release', '']);
check('a role without the required permission holds it once chosen',
  TokenForm.roleHeld(catalog, noRead[0]), ['tasks.read', 'boards.read']);
check('an unknown role name between two matches stays Custom with the hint',
  TokenForm.sync(catalog, ['boards.read', 'tasks.read'], roles, 'gone').hint,
  'These rows match the roles reviewer and release.');
check('a token whose rows differ from its role opens as Custom',
  TokenForm.sync(catalog, ['boards.read', 'tasks.read', 'tasks.bin'], noRead, 'verifier').role, null);

check('a custom set posts exactly the ticked names in catalog order',
  TokenForm.body(catalog, { name: 'x', ticked: ['tasks.create', 'boards.read', 'nope'], boards: null, expiryDays: 30 }),
  { name: 'x', permissions: ['boards.read', 'tasks.create'], boards: 'all', expires_in_days: 30 });
check('the lifetime menu offers 1 to 8 hours and marks 1 hour as the default',
  [TokenForm.lifetimeChoices().map((choice) => choice.days),
    TokenForm.lifetimeChoices()[0], TokenForm.lifetimeChoices()[7].label],
  [[1, 2, 3, 4, 5, 6, 7, 8], { days: 1, label: '1 hour', ends: 'Default' }, '8 hours']);
check('a chosen child lifetime posts as child_lifetime_hours',
  TokenForm.body(catalog, { name: 'x', ticked: ['tasks.read'], boards: null, expiryDays: 0, childLifetimeHours: 4 }).child_lifetime_hours, 4);
check('no chosen child lifetime posts no child_lifetime_hours',
  'child_lifetime_hours' in TokenForm.body(catalog, { name: 'x', ticked: ['tasks.read'], boards: null, expiryDays: 0 }), false);
check('a role posts its id with the permissions',
  TokenForm.body(catalog, { name: 'x', ticked: ['tasks.read'], roleId: 'r1', boards: null, expiryDays: 0 }).role_id, 'r1');
check('no role posts no role id',
  'role_id' in TokenForm.body(catalog, { name: 'x', ticked: ['tasks.read'], roleId: null, boards: null, expiryDays: 0 }), false);
check('the board picker posts the chosen ids',
  TokenForm.body(catalog, { name: 'x', ticked: ['tasks.read'], boards: ['b1', 'b2'], expiryDays: 0 }).boards,
  ['b1', 'b2']);


var boardList = [
  { id: 'b1', name: 'Roadmap' }, { id: 'b2', name: 'Road Trip' }, { id: 'b3', name: 'Chores' }
];
check('the board search matches a name part in any case',
  TokenForm.boardMatches(boardList, ' ROAD ').map(function (board) { return board.id; }), ['b1', 'b2']);
check('an empty board search lists every board', TokenForm.boardMatches(boardList, '').length, 3);
check('a board search with no hit lists none', TokenForm.boardMatches(boardList, 'zzz'), []);
check('the chips list the picked boards in board order',
  TokenForm.boardChips(boardList, ['b3', 'b1']).map(function (board) { return board.name; }),
  ['Roadmap', 'Chores']);
check('a picked board that no longer exists has no chip', TokenForm.boardChips(boardList, ['gone']), []);
check('toggling adds a board then removes it',
  [TokenForm.boardToggle([], 'b1'), TokenForm.boardToggle(['b1', 'b2'], 'b1')], [['b1'], ['b2']]);

var day = Date.UTC(2026, 10, 4);
check('each expiry choice shows the date it ends on',
  TokenForm.expiryChoices(day).map(function (choice) { return [choice.days, choice.label, choice.ends]; }),
  [[0, 'Never', 'No end date'], [30, '30 days', 'Ends 2026-12-04 UTC'],
    [90, '90 days', 'Ends 2027-02-02 UTC'], [365, '1 year', 'Ends 2027-11-04 UTC']]);
check('the default expiry is one of the choices',
  TokenForm.expiryChoices(day).some(function (choice) { return choice.days === TokenForm.expiryDefault; }), true);

var asked = ['boards.read', 'tasks.read', 'tasks.create'];
check('the device picker offers only the requested permissions',
  TokenForm.deviceCatalog(catalog, asked).map(function (item) { return item.name; }), asked);
check('the device boards keep only the matched ones',
  TokenForm.deviceBoards([{ id: 'b1', name: 'A' }, { id: 'b2', name: 'B' }, { id: 'b3', name: 'C' }], 'b3,b1').map(function (board) { return board.id; }), ['b1', 'b3']);
check('the device boards stay whole for a request naming none',
  TokenForm.deviceBoards([{ id: 'b1', name: 'A' }], '').length, 1);
var devicePage = fs.readFileSync('templates/device.html', 'utf8');
var at = function (text) { return devicePage.indexOf(text); };
check('the device page shows the summary, then Approve and Deny, before the edit controls',
  at('id="device-summary"') > 0 && at('id="device-summary"') < at('>Deny<') && at('>Deny<') < at('id="device-edit"')
    && at('>Approve<') < at('id="device-edit"') && at('id="device-picker"') > at('id="device-edit"'), true);
check('the device edit panel starts hidden',
  /id="device-edit" hidden/.test(devicePage), true);
var deviceRoleList = [
  { id: 'r1', name: 'reviewer', permissions: ['tasks.read', 'tasks.create'] },
  { id: 'r2', name: 'lead', permissions: ['tasks.read', 'tasks.bin'] },
  { id: 'r3', name: 'tiny', permissions: ['tasks.read'] }
];
check('the device picker keeps only roles within the request',
  TokenForm.deviceRoles(catalog, deviceRoleList, asked).map(function (role) { return role.id; }), ['r1', 'r3']);
check('a request without the required permission leaves no role',
  TokenForm.deviceRoles(catalog, deviceRoleList, ['tasks.read', 'tasks.create']), []);
check('the device fields post the permissions in catalog order, the boards, the role and the expiry',
  TokenForm.deviceFields(catalog, {
    userCode: 'ABCD-EFGH', ticked: ['tasks.create', 'boards.read'], boards: ['b1', 'b2'], roleId: 'r1', expiryDays: 2160
  }),
  [['user_code', 'ABCD-EFGH'], ['permissions_listed', '1'], ['permissions', 'boards.read'],
    ['permissions', 'tasks.create'], ['boards', 'b1'], ['boards', 'b2'], ['role_id', 'r1'], ['expires_in_hours', '2160']]);
check('the device fields post never for zero hours',
  TokenForm.deviceFields(catalog, { userCode: 'X', ticked: [], boards: null, roleId: null, expiryDays: 0 }),
  [['user_code', 'X'], ['permissions_listed', '1'], ['expires_in_hours', 'never']]);
check('the device fields post no boards for all boards and no role id for no role',
  TokenForm.deviceFields(catalog, { userCode: 'X', ticked: [], boards: null, roleId: null, expiryDays: 24 }),
  [['user_code', 'X'], ['permissions_listed', '1'], ['expires_in_hours', '24']]);
var deviceChoices = function (requested) {
  return TokenForm.deviceExpiryChoices(requested)(day).map(function (choice) {
    return [choice.days, choice.label, choice.ends];
  });
};
check('the device menu offers 1 hour, 8 hours, 1 day, 30 days, 90 days, 365 days and never for a request naming no hours',
  deviceChoices(0),
  [[1, '1 hour', 'Ends 2026-11-04 UTC'], [8, '8 hours', 'Ends 2026-11-04 UTC'],
    [24, '1 day', 'Ends 2026-11-05 UTC'], [720, '30 days', 'Ends 2026-12-04 UTC'],
    [2160, '90 days', 'Ends 2027-02-02 UTC'], [8760, '1 year', 'Ends 2027-11-04 UTC'],
    [0, 'Never', 'No end date']]);
check('a request for 8 hours offers 1 hour and 8 hours and no never',
  deviceChoices(8), [[1, '1 hour', 'Ends 2026-11-04 UTC'], [8, '8 hours', 'Ends 2026-11-04 UTC']]);
check('a request for 1 hour offers only 1 hour',
  deviceChoices(1).map(function (choice) { return choice[0]; }), [1]);
check('a request for 48 hours offers the shorter choices and the 48 hours',
  deviceChoices(48).map(function (choice) { return choice[0]; }), [1, 8, 24, 48]);
check('every device choice stays within the request',
  deviceChoices(1000).every(function (choice) { return choice[0] <= 1000; }), true);
check('the Settings choices stay without a one day choice',
  TokenForm.expiryChoices(day).some(function (choice) { return choice.days === 1; }), false);
check('a role token reaches a board when it is all boards or holds it',
  TokenForm.reachingTokens(['all', 'b1,b2', 'b3', ''], 'b2'), 2);
check('an all-boards role token reaches a board it does not list',
  TokenForm.reachingTokens(['all', 'b1'], 'b9'), 1);
check('the remove board message names zero tokens plainly',
  TokenForm.removeBoardMessage('lead', 'Beta', 0),
  'Remove "Beta" from "lead"? No live token of this role reaches Beta, so none will stop working.');
check('the remove board message names one token as it',
  TokenForm.removeBoardMessage('lead', 'Beta', 1),
  'Remove "Beta" from "lead"? 1 live token of this role reaches Beta, and it will stop working on every board, along with the tokens it issued. Issue a new one afterwards.');
check('the remove board message names several tokens as they',
  TokenForm.removeBoardMessage('lead', 'Beta', 2),
  'Remove "Beta" from "lead"? 2 live tokens of this role reach Beta, and they will stop working on every board, along with the tokens they issued. Issue new ones afterwards.');

if (failures) process.exit(1);
