const { previewCell } = require('../../static/pure.js');

let failures = 0;
function check(name, got, want) {
  if (got !== want) { failures++; console.log('FAIL ' + name + ': got ' + JSON.stringify(got) + ' want ' + JSON.stringify(want)); }
}

check('preview_cell_should_show_none_for_null', previewCell(null), '(none)');
check('preview_cell_should_keep_a_string_unchanged', previewCell('Done'), 'Done');

if (failures) process.exit(1);
console.log('bulk_preview_cell: all checks passed');
