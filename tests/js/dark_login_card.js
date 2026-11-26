const { check, finish } = require('./support/check.js');
var fs = require('fs');
var path = require('path');
var css = fs.readFileSync(path.join(path.dirname(process.argv[2]), 'style.css'), 'utf8');

function rule(selector) {
  var at = css.indexOf('\n' + selector + ' {');
  if (at < 0) return null;
  return css.slice(at, css.indexOf('}', at));
}

var dark = css.match(/body\.dark \.login-card select,\s*body\.dark \.login-card \.btn:not\(\.btn-primary\) \{[^}]*\}/);
check('dark selects and secondary buttons are themed', dark !== null &&
  /background: #363636/.test(dark[0]) && /color: var\(--ink\)/.test(dark[0]), true);
var avatar = rule('.tm-comment-avatar');
check('comment avatar initials are 12px', avatar !== null && /font-size: 12px/.test(avatar), true);
finish();
