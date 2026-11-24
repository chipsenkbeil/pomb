const fs = require('fs');
const { check, finish } = require('./support/check.js');
const src = fs.readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
const pull = (name) => {
  const head = `const ${name} = `;
  const start = src.indexOf(head) + head.length;
  const end = src.indexOf('\n  };\n', start) + '\n  }'.length;
  if (start < head.length || end < start) throw new Error(`${name} not found`);
  return eval('(' + src.slice(start, end) + ')');
};
const TabKeys = pull('TabKeys');
const PaneTabs = pull('PaneTabs');

const suite = 'pane_sub_tabs_should_show_one_panel_at_a_time';

const ids = ['a', 'b', 'c'];
const tabs = ids.map((id, i) => ({
  dataset: { paneTab: id },
  attrs: { 'aria-selected': i === 0 ? 'true' : 'false' },
  tabIndex: i === 0 ? 0 : -1,
  setAttribute(name, value) { this.attrs[name] = value; },
}));
const panels = ids.map((id, i) => ({ dataset: { panePanel: id }, hidden: i !== 0 }));
const visible = () => panels.filter((panel) => !panel.hidden).map((panel) => panel.dataset.panePanel);
const selected = () => tabs.filter((tab) => tab.attrs['aria-selected'] === 'true').map((tab) => tab.dataset.paneTab);

check('the first panel is shown on load', visible(), ['a']);
PaneTabs.show(tabs, panels, 'b');
check('choosing a tab shows only that panel', visible(), ['b']);
check('choosing a tab selects only that tab', selected(), ['b']);
check('only the chosen tab is in the tab order', tabs.map((tab) => tab.tabIndex), [-1, 0, -1]);
PaneTabs.show(tabs, panels, TabKeys.step(ids, 'c', 'ArrowRight'));
check(`${suite}: ArrowRight wraps and shows only the first panel`, visible(), ['a']);
PaneTabs.show(tabs, panels, TabKeys.step(ids, 'a', 'ArrowLeft'));
check(`${suite}: ArrowLeft wraps and shows only the last panel`, visible(), ['c']);
PaneTabs.show(tabs, panels, TabKeys.step(ids, 'b', 'Home'));
check('Home shows the first panel', visible(), ['a']);
PaneTabs.show(tabs, panels, TabKeys.step(ids, 'a', 'End'));
check('End shows the last panel', visible(), ['c']);
check('the step helper ignores other keys', TabKeys.step(ids, 'a', 'Enter'), null);
const field = (id) => ({ closest: () => ({ dataset: { panePanel: id } }) });
check('an invalid field names its panel', PaneTabs.owner(field('b')), 'b');
check('a field outside the panels names none', PaneTabs.owner({ closest: () => null }), null);

finish();
