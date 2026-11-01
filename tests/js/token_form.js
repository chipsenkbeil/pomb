var fs = require('fs');
var src = fs.readFileSync('static/pages.js', 'utf8');
var start = src.indexOf('  var TokenForm = {');
var end = src.indexOf('\n  };\n', start) + '\n  };\n'.length;
if (start < 0 || end < start) throw new Error('TokenForm not found');
eval(src.slice(start, end));

var catalog = [
  { name: 'a.read', group: 'a', description: '', presets: ['low', 'mid', 'top'] },
  { name: 'a.write', group: 'a', description: '', presets: ['mid', 'top'] },
  { name: 'b.drop', group: 'b', description: '', presets: ['top'] },
  { name: 'b.read', group: 'b', description: '', presets: ['low', 'mid', 'top'] }
];

var failures = 0;
function check(name, got, want) {
  var ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got)));
}

check('groups keep catalog order',
  TokenForm.groups(catalog).map(function (g) { return g.name + ':' + g.items.length; }), ['a:2', 'b:2']);
check('presets run smallest first',
  TokenForm.presets(catalog).map(function (p) { return p.name; }), ['low', 'mid', 'top']);
check('picking a preset ticks exactly its permissions',
  TokenForm.presets(catalog)[1].permissions, ['a.read', 'a.write', 'b.read']);
check('a custom set posts exactly the ticked names in catalog order',
  TokenForm.body(catalog, { name: 'x', ticked: ['b.read', 'a.write', 'nope'], boards: null, expiryDays: 30 }),
  { name: 'x', permissions: ['a.write', 'b.read'], boards: 'all', expires_in_days: 30 });
check('the board picker posts the chosen ids',
  TokenForm.body(catalog, { name: 'x', ticked: ['a.read'], boards: ['b1', 'b2'], expiryDays: 0 }).boards,
  ['b1', 'b2']);

if (failures) process.exit(1);
