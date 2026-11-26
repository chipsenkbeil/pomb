const { previewCell } = require('../../static/pure.js');

const { check, finish } = require('./support/check.js');

check('preview_cell_should_show_none_for_null', previewCell(null), '(none)');
check('preview_cell_should_keep_a_string_unchanged', previewCell('Done'), 'Done');

finish();
