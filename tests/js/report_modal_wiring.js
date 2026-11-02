var src = require('fs').readFileSync(process.argv[2], 'utf8');
function fn(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}

var listeners = {};
var reportPage = true;
var present = {};
var relationInits = 0;
function element(id) {
  return {
    hidden: false,
    addEventListener: function (type, handler) {
      listeners[id + ':' + type] = (listeners[id + ':' + type] || []).concat(handler);
    }
  };
}
var document = {
  getElementById: function (id) { return present[id] || null; },
  querySelector: function (sel) { return sel === '.bshell-report' && reportPage ? {} : null; }
};
function doMoveTask() {}
function doCopyTask() {}
function doAddEstimate() {}
function initRelationDialog() { relationInits++; }

var taskDialogsInitialized = false;
eval(fn('  function initTaskDialogs() {'));
eval(fn('  function initReportModal() {'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function count(key) { return (listeners[key] || []).length; }

['mt-move-btn', 'cp-copy-btn', 'cp-name', 'move-task-dialog', 'est-add', 'estimate-dialog', 'duedate-dialog'].forEach(function (id) {
  present[id] = element(id);
});

initReportModal();
check('move button is wired on a report page', count('mt-move-btn:click'), 1);
check('copy button is wired on a report page', count('cp-copy-btn:click'), 1);
check('estimate add is wired on a report page', count('est-add:click'), 1);
check('move overlay closes on outside click', count('move-task-dialog:click'), 1);
check('estimate overlay closes on outside click', count('estimate-dialog:click'), 1);
check('due overlay closes on outside click', count('duedate-dialog:click'), 1);
check('relation dialog is wired', relationInits, 1);

var closed = { hidden: false, classList: { contains: function () { return true; } } };
listeners['move-task-dialog:click'][0]({ target: closed, currentTarget: closed });
check('outside click hides the move popover', closed.hidden, true);

initReportModal();
initTaskDialogs();
check('wiring runs once', count('mt-move-btn:click'), 1);

listeners = {};
taskDialogsInitialized = false;
reportPage = false;
initReportModal();
check('a non-report page is left to the board wiring', count('mt-move-btn:click'), 0);

reportPage = true;
present = {};
initReportModal();
check('a report page without the dialogs is skipped', count('mt-move-btn:click'), 0);

var boardMenus = src.slice(src.indexOf('  function initBoardMenus() {'));
boardMenus = boardMenus.slice(0, boardMenus.indexOf('\n  }\n'));
check('the board wiring goes through initTaskDialogs', boardMenus.indexOf('initTaskDialogs();') >= 0, true);
check('the board wiring does not bind the move button itself', boardMenus.indexOf("'mt-move-btn'"), -1);

console.log('failures=' + failures);
process.exit(failures ? 1 : 0);
