const fs = require('fs');
const path = require('path');
const { check, finish } = require('./support/check.js');

const css = fs.readFileSync(path.join(path.dirname(process.argv[2]), 'style.css'), 'utf8');

const lit = css.indexOf('\n#filter-btn.active {');
const filtering = css.indexOf('\n#filter-btn.filtering {');
check('lit_when_open', lit >= 0, true);
check('filtering_wins', lit >= 0 && filtering > lit, true);
finish();
