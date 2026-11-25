const src = require('fs').readFileSync(process.argv[2], 'utf8');
const start = src.indexOf('  var BoardChrome = {');
const end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('BoardChrome not found');

const { check, finish } = require('./support/check.js');

const node = () => {
  const props = {};
  const el = {
    children: [],
    dataset: {},
    attrs: {},
    className: '',
    props,
    style: { setProperty: (name, value) => { props[name] = value; } },
    classList: { add: (name) => { el.className += ' ' + name; } },
    appendChild: (child) => { el.children.push(child); return child; },
    setAttribute: (name, value) => { el.attrs[name] = value; },
    addEventListener: () => {},
  };
  return el;
};

const all = (el, cls) => {
  const own = (' ' + el.className + ' ').includes(' ' + cls + ' ') ? [el] : [];
  return own.concat(...el.children.map((child) => all(child, cls)));
};

const document = { createElement: node, getElementById: () => null, querySelectorAll: () => [] };
const chrome = eval('(function () {' + src.slice(start, end) + '\n return BoardChrome; })()');

const epic = (flags) => ({ id: 'e1', name: 'One', closed: false, flags });

const flagged = chrome.epicRow(epic([{ label: 'a', color: '#b8461f' }, { label: 'b', color: '#2d6cc0' }]), null, []);
check('epic_row_flags the first flag colours the stripe', flagged.props['--hl-stripe'], '#b8461f');
check('epic_row_flags the row carries the stripe class', flagged.className.includes('epic-row-hl'), true);
const icons = all(flagged, 'epic-flag');
check('epic_row_flags one flag per entry', icons.map((icon) => [icon.dataset.flagLabel, icon.style.color]), [['a', '#b8461f'], ['b', '#2d6cc0']]);

const plain = chrome.epicRow(epic([]), null, []);
check('epic_row_flags an epic with none keeps its row', [plain.props['--hl-stripe'], plain.className.includes('epic-row-hl'), all(plain, 'epic-flag').length], [undefined, false, 0]);

const old = chrome.epicRow({ id: 'e2', name: 'Two', closed: false }, null, []);
check('epic_row_flags a list without flags keeps its row', all(old, 'epic-flag').length, 0);

finish();
