const { BULK_ACT } = require('../../static/pure.js');
const { check, finish } = require('./support/check.js');
const src = require('fs').readFileSync(process.argv[2], 'utf8');
function slice(from, to) {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
}

const body = slice('function openBulkMenu(x, y) {', 'function bulkSubmenuButton(');

function makeMenu(empty) {
  const buttons = {};
  const selectors = [];
  return {
    buttons,
    selectors,
    querySelector(sel) {
      selectors.push(sel);
      if (empty) return null;
      if (!buttons[sel]) buttons[sel] = { hidden: false };
      return buttons[sel];
    },
  };
}

function openWith(acts, cards, empty) {
  const menu = makeMenu(empty);
  const placed = [];
  const document = { getElementById: (id) => (id === 'bulk-ctx-menu' ? menu : null) };
  const open = new Function(
    'BULK_ACT', 'selectedCards', 'placeMenu', 'document',
    'var bulkCards = [];\n' + body + '\nreturn openBulkMenu;'
  )(acts, () => cards, () => placed.push(1), document);
  open(3, 4);
  return { menu, placed };
}

const memberCard = { dataset: { memberIds: 'u1' } };
const plainCard = { dataset: { memberIds: '' } };

const fake = { MEMBERS: 'm', MEMBERS_MENU: 'mm' };
const named = openWith(fake, [plainCard]);
check('bulk_menu_members_should_select_both_buttons_through_the_constants', named.menu.selectors,
  ['[data-bulk-act]', '[data-bulk-act="m"]', '[data-bulk-act="mm"]']);

const viewer = openWith(BULK_ACT, [plainCard], true);
check('bulk_menu_members_should_leave_an_empty_menu_closed', viewer.placed.length, 0);

const withMembers = openWith(BULK_ACT, [plainCard, memberCard]);
check('bulk_menu_members_should_hide_assign_members_when_a_card_holds_members',
  withMembers.menu.buttons['[data-bulk-act="members"]'].hidden, true);
check('bulk_menu_members_should_show_members_when_a_card_holds_members',
  withMembers.menu.buttons['[data-bulk-act="members-menu"]'].hidden, false);

const noMembers = openWith(BULK_ACT, [plainCard, plainCard]);
check('bulk_menu_members_should_show_assign_members_when_no_card_holds_members',
  noMembers.menu.buttons['[data-bulk-act="members"]'].hidden, false);
check('bulk_menu_members_should_hide_members_when_no_card_holds_members',
  noMembers.menu.buttons['[data-bulk-act="members-menu"]'].hidden, true);

finish();
