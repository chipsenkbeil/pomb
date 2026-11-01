var src = require('fs').readFileSync(process.argv[2], 'utf8');
function slice(from, to) {
  var start = src.indexOf(from);
  var end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
}

var toasts = [];
var patches = [];
var nameValue = '';
var dialog = { hidden: false };
var description = { value: '' };
var editLaneId = 'lane1';
var editLaneLimits = [];
var editLaneDescription = '';
var document = {
  getElementById: function (id) {
    if (id === 'le-name') return { value: nameValue };
    if (id === 'le-description') return description;
    return dialog;
  }
};
function toast(msg) { toasts.push(msg); }
function patchLane(id, patch) { patches.push([id, patch.name]); }
eval(slice('  function doSaveLane() {', '\n  function refreshLaneCounts'));
eval(slice('  function onLaneNameKeydown(', '\n  }\n') + '\n  }\n');

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function press(key) {
  var prevented = false;
  onLaneNameKeydown({ key: key, preventDefault: function () { prevented = true; } });
  return prevented;
}

nameValue = '  Ops  ';
check('Enter in the rename name field is consumed', press('Enter'), true);
check('Enter in the rename name field saves the trimmed name', patches, [['lane1', 'Ops']]);
check('Enter in the rename name field closes the dialog', dialog.hidden, true);

patches.length = 0; dialog.hidden = false; nameValue = '   ';
press('Enter');
check('Enter with an empty name shows the toast', toasts, ['Swimlane name is required.']);
check('Enter with an empty name saves nothing', patches, []);
check('Enter with an empty name keeps the dialog open', dialog.hidden, false);

toasts.length = 0; nameValue = 'Ops';
nameValue = 'Ops';
check('Enter while composing is not consumed', (function () {
  var prevented = false;
  onLaneNameKeydown({ key: 'Enter', isComposing: true, preventDefault: function () { prevented = true; } });
  return prevented;
})(), false);
check('Enter while composing saves nothing', patches, []);
check('Enter with keyCode 229 is not consumed', (function () {
  var prevented = false;
  onLaneNameKeydown({ key: 'Enter', keyCode: 229, preventDefault: function () { prevented = true; } });
  return prevented;
})(), false);
check('Enter with keyCode 229 saves nothing', patches, []);

check('another key is not consumed', press('a'), false);
check('another key saves nothing', patches, []);

check('the name field is wired to the handler',
  /getElementById\('le-name'\)\.addEventListener\('keydown', onLaneNameKeydown\)/.test(src), true);

if (failures) process.exit(1);
console.log('lane_rename_enter ok');
