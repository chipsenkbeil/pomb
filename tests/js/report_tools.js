var src = require('fs').readFileSync(process.argv[2], 'utf8');
function block(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  var to = src.indexOf('\n  })();', from) + '\n  })();'.length;
  return src.slice(from, to);
}
function escapeHtml(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}
var ReportFile = null;
var saveBlob = function () {};
var toast = function () {};
eval(block('  var ReportTools = (function () {'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

check('the list endpoint', ReportTools.endpoint('b 1'), '/api/boards/b%201/report-bookmarks');
check('a bookmark endpoint', ReportTools.endpoint('b1', 'k/2'), '/api/boards/b1/report-bookmarks/k%2F2');
check('the bookmark link opens the report with the id',
  ReportTools.bookmarkPath('b1', 'cumulative-flow', 'k2'), '/b/b1/cumulative-flow?bookmark=k2');
check('the id is read from the query', ReportTools.bookmarkIdOf('?x=1&bookmark=k%202&y=2'), 'k 2');
check('no bookmark in the query', ReportTools.bookmarkIdOf('?x=1'), '');
check('an empty query', ReportTools.bookmarkIdOf(''), '');

var calls = [];
global.fetch = function (url, init) {
  calls.push([url, init.method, init.body || null]);
  return Promise.resolve({
    ok: init.method !== 'PATCH',
    json: function () { return Promise.resolve({ done: true }); },
    text: function () { return Promise.resolve('{"message":"name too long"}'); },
    status: 400,
  });
};
Promise.all([
  ReportTools.send('POST', '/x', { a: 1 }),
  ReportTools.send('DELETE', '/y'),
  ReportTools.send('PATCH', '/z', {}).then(function () { return 'ok'; }, function (e) { return e.message; }),
]).then(function (out) {
  check('a write returns the JSON', out[0], { done: true });
  check('a delete returns nothing', out[1], null);
  check('a refusal carries the server message', out[2], 'name too long');
  check('the requests', calls, [['/x', 'POST', '{"a":1}'], ['/y', 'DELETE', null], ['/z', 'PATCH', '{}']]);
  if (failures) process.exit(1);
  console.log('report_tools: all checks passed');
});
