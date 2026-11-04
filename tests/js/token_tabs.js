var fs = require('fs');
var src = fs.readFileSync('static/pages.js', 'utf8');
var start = src.indexOf('  var TokenTabs = {');
var end = src.indexOf('\n  };\n', start) + '\n  };\n'.length;
if (start < 0 || end < start) throw new Error('TokenTabs not found');
eval(src.slice(start, end));

var failures = 0;
function check(name, got, want) {
  var ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got)));
}

check('the bare section hash opens Create', TokenTabs.fromHash('#api-tokens'), 'create');
check('the list hash opens List and revoke', TokenTabs.fromHash('#api-tokens/list'), 'list');
check('the debug hash opens Debug', TokenTabs.fromHash('#api-tokens/debug'), 'debug');
check('an unknown tab opens Create', TokenTabs.fromHash('#api-tokens/nope'), 'create');
check('another section opens Create', TokenTabs.fromHash('#passkeys/list'), 'create');
check('no hash opens Create', TokenTabs.fromHash(''), 'create');
check('Create writes the bare section hash', TokenTabs.toHash('create'), '#api-tokens');
check('the other tabs write a sub path', [TokenTabs.toHash('debug'), TokenTabs.toHash('list')],
  ['#api-tokens/debug', '#api-tokens/list']);
check('every tab survives a hash round trip',
  TokenTabs.names.map(function (name) { return TokenTabs.fromHash(TokenTabs.toHash(name)); }), TokenTabs.names);
check('the pane is the part before the slash',
  [TokenTabs.paneOf('#api-tokens/list'), TokenTabs.paneOf('#api-tokens'), TokenTabs.paneOf('')],
  ['api-tokens', 'api-tokens', '']);
check('ArrowRight moves on and wraps',
  [TokenTabs.step('create', 'ArrowRight'), TokenTabs.step('list', 'ArrowRight')], ['debug', 'create']);
check('ArrowLeft moves back and wraps',
  [TokenTabs.step('debug', 'ArrowLeft'), TokenTabs.step('create', 'ArrowLeft')], ['create', 'list']);
check('Home and End jump to the ends',
  [TokenTabs.step('debug', 'Home'), TokenTabs.step('debug', 'End')], ['create', 'list']);
check('other keys do nothing', TokenTabs.step('create', 'Enter'), null);

if (failures) process.exit(1);
