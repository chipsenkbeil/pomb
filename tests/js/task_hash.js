var src = require('fs').readFileSync(process.argv[2], 'utf8');
function fn(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}

var onBoard = true;
var location = { pathname: '/b/b1', search: '?q=x', hash: '' };
var pushes = 0;
var window = {
  location: location,
  history: {
    state: { keep: 1 },
    replaceState: function (state, title, url) {
      var at = url.indexOf('#');
      location.hash = at < 0 ? '' : url.slice(at);
    },
    pushState: function () { pushes++; }
  }
};
function boardId() { return onBoard ? 'b1' : null; }
var TASK_HASH = '#task-';
eval(fn('  function showTaskHash(taskId) {'));
eval(fn('  function clearTaskHash() {'));

var failures = 0;
function check(name, got, want) {
  if (got !== want) { failures++; console.log('FAIL ' + name + ': got ' + got + ' want ' + want); }
}

showTaskHash('t1');
check('opening a card sets its hash', location.hash, '#task-t1');
showTaskHash('t2');
check('opening another card replaces the hash', location.hash, '#task-t2');
clearTaskHash();
check('closing clears the hash', location.hash, '');
location.hash = '#find-t9';
showTaskHash('t1');
check('a find hash is left alone', location.hash, '#find-t9');
clearTaskHash();
check('closing leaves a find hash', location.hash, '#find-t9');
location.hash = '#layout';
showTaskHash('t1');
check('a layout hash is left alone', location.hash, '#layout');
location.hash = '';
onBoard = false;
showTaskHash('t1');
check('a page without a board gets no hash', location.hash, '');
check('no history entry is pushed', pushes, 0);
console.log('failures=' + failures);
process.exit(failures ? 1 : 0);
