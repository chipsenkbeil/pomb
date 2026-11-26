const src = require('fs').readFileSync(process.argv[2], 'utf8');
const start = src.indexOf('  const syncDarkCookie = ');
const end = src.indexOf('\n  };\n', start) + '\n  };\n'.length;
if (start < 0 || end < start) throw new Error('syncDarkCookie not found');

const document = { cookie: '' };
new Function('document', 'DARK_COOKIE_MAX_AGE_SECS', `${src.slice(start, end)}\n  this.syncDarkCookie = syncDarkCookie;`).call(document, document, 31536000);

const { check, finish } = require('./support/check.js');

document.syncDarkCookie(true);
check('dark on sets pomb-dark=1', /^pomb-dark=1;/.test(document.cookie), true);
check('the cookie is site wide', /; Path=\//.test(document.cookie), true);
check('the cookie is SameSite=Lax', /; SameSite=Lax/.test(document.cookie), true);
check('the cookie lasts a year', /; Max-Age=31536000/.test(document.cookie), true);
check('the cookie holds nothing else', document.cookie.split(';')[0] === 'pomb-dark=1', true);
document.syncDarkCookie(false);
check('dark off clears the cookie', /^pomb-dark=;/.test(document.cookie) && /; Max-Age=0/.test(document.cookie), true);
finish();
