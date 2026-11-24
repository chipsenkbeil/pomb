const fs = require('fs');
const { check, finish } = require('./support/check.js');
const src = fs.readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
const start = src.indexOf('const RoleForm = ') + 'const RoleForm = '.length;
const end = src.indexOf('\n  };\n', start) + '\n  }'.length;
if (start < 'const RoleForm = '.length || end < start) throw new Error('RoleForm not found');
const RoleForm = eval('(' + src.slice(start, end) + ')');

const suite = 'role_form_should_keep_catalog_order';

const catalog = [{ name: 'cards.read' }, { name: 'cards.write' }, { name: 'boards.read' }];

check(`${suite}: the held permissions follow the catalog, not the pick order`,
  RoleForm.body('x', catalog, ['boards.read', 'cards.read']).permissions, ['cards.read', 'boards.read']);
check(`${suite}: the name is trimmed`, RoleForm.body('  reviewer \n', catalog, []).name, 'reviewer');
check(`${suite}: a held name outside the catalog is dropped`,
  RoleForm.body('x', catalog, ['nope', 'cards.write']).permissions, ['cards.write']);
check(`${suite}: nothing held gives an empty list`, RoleForm.body('x', catalog, []).permissions, []);

finish();
