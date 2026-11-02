var src = require('fs').readFileSync(process.argv[2], 'utf8');
function fn(start) {
  var from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}

var modal = null;
var pageBoard = null;
var chip = { hidden: true, textContent: '' };
var input = { value: 'x', focus: function () {} };
var picker = { hidden: true };
var elements = { 'tm-picker': picker, 'tm-picker-input': input, 'tm-picker-chip': chip };
var document = {
  activeElement: null,
  getElementById: function (id) { return elements[id] || null; },
  querySelector: function (sel) {
    if (sel === '#modal-root .task-modal') return modal;
    if (sel === '.board-bar-title') return pageBoard ? { textContent: ' ' + pageBoard.name + ' ' } : null;
    return null;
  }
};
var linkPicker = { mode: null, onPick: null, timer: null, seq: 0, opener: null, board: null };
function boardId() { return pageBoard ? pageBoard.id : null; }
function loadLinkPickerResults() {}
eval(fn('  function modalTaskBoard() {'));
eval(fn('  function paintPickerChip() {'));
eval(fn('  function openLinkPicker(mode, onPick) {'));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

pageBoard = { id: 'page', name: 'Page board' };
modal = { dataset: { boardId: 'task', boardName: 'Task board' } };
openLinkPicker('child', function () {});
check('the chip follows the task board, not the page', linkPicker.board, { id: 'task', name: 'Task board' });
check('the chip shows the task board name', chip.textContent, 'board:Task board');

modal = null;
openLinkPicker('child', function () {});
check('without a modal board the chip falls back to the page', linkPicker.board, { id: 'page', name: 'Page board' });

modal = { dataset: {} };
pageBoard = null;
openLinkPicker('child', function () {});
check('with neither there is no chip', linkPicker.board, null);
check('with neither the chip is hidden', chip.hidden, true);

console.log('failures=' + failures);
if (failures) throw new Error(failures + ' link picker board check(s) failed');
