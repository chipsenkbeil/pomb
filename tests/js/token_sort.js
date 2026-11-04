var fs = require('fs');
var src = fs.readFileSync('static/pages.js', 'utf8');
var start = src.indexOf('  var TokenSort = {');
var end = src.indexOf('\n  };\n', start) + '\n  };\n'.length;
if (start < 0 || end < start) throw new Error('TokenSort not found');
eval(src.slice(start, end));

var failures = 0;
function check(name, got, want) {
  var ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got)));
}

function row(id, name, token, scope, created, lastUsed, expires) {
  return { id: id, name: name, token: token, scope: scope, created: created, last_used: lastUsed, expires: expires };
}
var rows = [
  row('a', 'beta', 'pomb_b…2222', 'write', '200', '', '3000'),
  row('b', 'Alpha', 'pomb_a…9999', 'read', '1000', '500', ''),
  row('c', 'gamma', 'pomb_c…1111', 'admin', '30', '90', '40'),
  row('d', 'delta', 'pomb_d…5555', 'read', '4', '', '')
];
function ids(column, direction) {
  return TokenSort.order(rows, column, direction).map(function (r) { return r.id; });
}

check('name ascending ignores case', ids('name', 'ascending'), ['b', 'a', 'd', 'c']);
check('name descending', ids('name', 'descending'), ['c', 'd', 'a', 'b']);
check('token ascending', ids('token', 'ascending'), ['b', 'a', 'c', 'd']);
check('token descending', ids('token', 'descending'), ['d', 'c', 'a', 'b']);
check('scope ascending keeps ties in order', ids('scope', 'ascending'), ['c', 'b', 'd', 'a']);
check('scope descending keeps ties in order', ids('scope', 'descending'), ['a', 'b', 'd', 'c']);
check('created sorts as numbers', ids('created', 'ascending'), ['d', 'c', 'a', 'b']);
check('created descending', ids('created', 'descending'), ['b', 'a', 'c', 'd']);
check('never used sorts last ascending', ids('last_used', 'ascending'), ['c', 'b', 'a', 'd']);
check('never used sorts last descending', ids('last_used', 'descending'), ['b', 'c', 'a', 'd']);
check('never expires sorts last ascending', ids('expires', 'ascending'), ['c', 'a', 'b', 'd']);
check('never expires sorts last descending', ids('expires', 'descending'), ['a', 'c', 'b', 'd']);
check('order leaves the input untouched', rows.map(function (r) { return r.id; }), ['a', 'b', 'c', 'd']);
check('a first click sorts ascending', TokenSort.toggle(null, 'name'), { column: 'name', direction: 'ascending' });
check('a second click reverses',
  TokenSort.toggle({ column: 'name', direction: 'ascending' }, 'name'), { column: 'name', direction: 'descending' });
check('a third click goes back',
  TokenSort.toggle({ column: 'name', direction: 'descending' }, 'name'), { column: 'name', direction: 'ascending' });
check('another column starts ascending',
  TokenSort.toggle({ column: 'name', direction: 'descending' }, 'scope'), { column: 'scope', direction: 'ascending' });
check('the columns match the table', TokenSort.columns, ['name', 'token', 'scope', 'created', 'last_used', 'expires']);

if (failures) process.exit(1);
