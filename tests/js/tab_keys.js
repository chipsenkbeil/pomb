const fs = require('fs');
const { check, finish } = require('./support/check.js');
const src = fs.readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
const start = src.indexOf('const TabKeys = ') + 'const TabKeys = '.length;
const end = src.indexOf('\n  };\n', start) + '\n  }'.length;
if (start < 'const TabKeys = '.length || end < start) throw new Error('TabKeys not found');
const TabKeys = eval('(' + src.slice(start, end) + ')');

const suite = 'tab_keys_should_wrap_and_ignore_other_keys';
const names = ['a', 'b', 'c'];

check(`${suite}: ArrowRight moves on and wraps`,
  [TabKeys.step(names, 'a', 'ArrowRight'), TabKeys.step(names, 'c', 'ArrowRight')], ['b', 'a']);
check(`${suite}: ArrowLeft moves back and wraps`,
  [TabKeys.step(names, 'b', 'ArrowLeft'), TabKeys.step(names, 'a', 'ArrowLeft')], ['a', 'c']);
check(`${suite}: Home and End jump to the ends`,
  [TabKeys.step(names, 'b', 'Home'), TabKeys.step(names, 'b', 'End')], ['a', 'c']);
check(`${suite}: other keys do nothing`, TabKeys.step(names, 'a', 'Enter'), null);

finish();
