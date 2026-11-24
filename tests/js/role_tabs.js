const fs = require('fs');
const { check, finish } = require('./support/check.js');
const src = fs.readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
const keysStart = src.indexOf('const TabKeys = ') + 'const TabKeys = '.length;
const keysEnd = src.indexOf('\n  };\n', keysStart) + '\n  }'.length;
if (keysStart < 'const TabKeys = '.length || keysEnd < keysStart) throw new Error('TabKeys not found');
const TabKeys = eval('(' + src.slice(keysStart, keysEnd) + ')');
const start = src.indexOf('const RoleTabs = ') + 'const RoleTabs = '.length;
const end = src.indexOf('\n  };\n', start) + '\n  }'.length;
if (start < 'const RoleTabs = '.length || end < start) throw new Error('RoleTabs not found');
const RoleTabs = eval('(' + src.slice(start, end) + ')');

const suite = 'role_tabs_should_show_one_role_at_a_time';

const ids = ['a', 'b', 'c'];
const tabs = ids.map((id, i) => ({
  dataset: { roleTab: id },
  attrs: { 'aria-selected': i === 0 ? 'true' : 'false' },
  tabIndex: i === 0 ? 0 : -1,
  setAttribute(name, value) { this.attrs[name] = value; },
}));
const cards = ids.map((id, i) => ({ dataset: { roleId: id }, hidden: i !== 0 }));
const visible = () => cards.filter((card) => !card.hidden).map((card) => card.dataset.roleId);
const selected = () => tabs.filter((tab) => tab.attrs['aria-selected'] === 'true').map((tab) => tab.dataset.roleTab);

check('the first role is shown on load', visible(), ['a']);
RoleTabs.show(tabs, cards, 'b');
check('choosing a tab shows only that role card', visible(), ['b']);
check('choosing a tab selects only that tab', selected(), ['b']);
check('only the chosen tab is in the tab order', tabs.map((tab) => tab.tabIndex), [-1, 0, -1]);
check('ArrowRight moves on and wraps',
  [RoleTabs.step(ids, 'a', 'ArrowRight'), RoleTabs.step(ids, 'c', 'ArrowRight')], ['b', 'a']);
check('ArrowLeft moves back and wraps',
  [RoleTabs.step(ids, 'b', 'ArrowLeft'), RoleTabs.step(ids, 'a', 'ArrowLeft')], ['a', 'c']);
check('Home and End jump to the ends',
  [RoleTabs.step(ids, 'b', 'Home'), RoleTabs.step(ids, 'b', 'End')], ['a', 'c']);
check('other keys do nothing', RoleTabs.step(ids, 'a', 'Enter'), null);
RoleTabs.show(tabs, cards, RoleTabs.step(ids, 'b', 'ArrowRight'));
check(`${suite}: an arrow key shows only the next role card`, visible(), ['c']);

finish();
