var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  function refreshAfterMemberChange(');
var end = src.indexOf('\n  }\n', start) + '\n  }\n'.length;
if (start < 0 || end < start) { console.log('FAIL refreshAfterMemberChange not found'); process.exit(1); }

var calls = [];
var dialog = { hidden: true };
var search = { value: 'ann', focus: function () { calls.push('focus'); } };
var document = {
  getElementById: function (id) { return id === 'members-dialog' ? dialog : search; },
};
var TimerUI = { refreshTaskCard: function (id) { calls.push('card:' + id); } };
var modal = null;
var modalDirty = false;
function modalTaskId() { return modal; }
function renderMembersList(filter) { calls.push('list:' + filter); }
function openModal(id) { calls.push('modal:' + id); }
eval(src.slice(start, end));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

refreshAfterMemberChange('t1');
check('refresh re-renders the list and the card when the modal is closed',
  calls, ['list:ann', 'card:t1']);
check('refresh leaves the dialog alone when the modal is closed', dialog.hidden, true);

calls.length = 0;
modal = 't1';
refreshAfterMemberChange('t1');
check('refresh reopens the modal and keeps the dialog on top',
  calls, ['list:ann', 'card:t1', 'modal:t1', 'focus']);
check('refresh marks the modal dirty and shows the dialog', [modalDirty, dialog.hidden], [true, false]);

calls.length = 0;
modal = 't2';
refreshAfterMemberChange('t1');
check('refresh skips the modal when it shows another task', calls, ['list:ann', 'card:t1']);

if (failures) process.exit(1);
console.log('member_refresh ok');
