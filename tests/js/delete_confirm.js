var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  function deleteConfirmMessage(');
var end = src.indexOf('\n  }\n', start) + '\n  }\n'.length;
if (start < 0 || end < start) throw new Error('deleteConfirmMessage not found');
var headers = {
  todo: { dataset: { isDone: '0' } },
  finished: { dataset: { isDone: '1' } },
  Done: { dataset: { isDone: '0' } },
};
var document = {
  querySelector: function (selector) {
    var id = /data-column-id="([^"]*)"/.exec(selector)[1];
    return headers[id] || null;
  },
};
function cssEscape(s) { return String(s); }
eval(src.slice(start, end));

function cardIn(columnId) {
  return { closest: function () { return { dataset: { columnId: columnId } }; } };
}

var failures = 0;
function check(name, got, want) {
  var ok = got === want;
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got)));
}

var plain = 'Delete "Ship it"?';
check('a done column warns', deleteConfirmMessage('Ship it', cardIn('finished')),
  plain + ' Deleted tasks are removed permanently.');
check('another column keeps the plain confirm', deleteConfirmMessage('Ship it', cardIn('todo')), plain);
check('a column named Done that is not a done column keeps the plain confirm',
  deleteConfirmMessage('Ship it', cardIn('Done')), plain);
check('no card keeps the plain confirm', deleteConfirmMessage('Ship it', null), plain);
process.exit(failures ? 1 : 0);
