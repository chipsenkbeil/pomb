const path = require('path');
const fs = require('fs');
const { check, finish } = require('./support/check.js');

const css = fs.readFileSync(path.join(path.dirname(process.argv[2]), 'style.css'), 'utf8');

const threeAcross = '#edit-column-dialog .dlg-grid2 { grid-template-columns: repeat(3, minmax(0, 1fr)); }';
const oneColumn = '#edit-column-dialog .dlg-grid2 { grid-template-columns: minmax(0, 1fr); }';
const selectRule = /\n#edit-column-dialog \.dlg-grid2 select \{([^}]*)\}/.exec(css);
const selectBody = selectRule ? selectRule[1] : '';

check('the dialog grid defaults to three shrinkable columns', css.includes(threeAcross), true);
check('the dialog selects may shrink below their content', /min-width:\s*0\b/.test(selectBody), true);
check('the dialog selects fill their column', /width:\s*100%/.test(selectBody), true);

const phoneStart = css.indexOf('@media (max-width: 639px) {', css.indexOf(threeAcross));
check('a phone block follows the three-column default', phoneStart >= 0, true);
const phoneBlock = phoneStart >= 0 ? css.slice(phoneStart, css.indexOf('\n}', phoneStart)) : '';
check('the phone block uses one column', phoneBlock.includes(oneColumn), true);

finish();
