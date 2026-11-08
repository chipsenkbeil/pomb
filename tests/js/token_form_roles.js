const fs = require('fs');
const src = fs.readFileSync('static/pages.js', 'utf8');
const start = src.indexOf('  var TokenForm = {');
const end = src.indexOf('\n  };\n', start) + '\n  };\n'.length;
if (start < 0 || end < start) throw new Error('TokenForm not found');
eval(src.slice(start, end));

let failures = 0;
function check(name, got, want) {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got)));
}

const roles = [
  { name: 'scribe', permissions: ['tasks.read'] },
  { name: 'planner', permissions: ['tasks.read'] },
  { name: 'lead', permissions: ['tasks.read'] },
];

Promise.all([
  TokenForm.loadRoles(() => Promise.reject(new Error('HTTP 500'))),
  TokenForm.loadRoles(() => Promise.resolve(roles)),
]).then((outcomes) => {
  check('token_form_shows_error_when_roles_load_fails: the error names the failure',
    outcomes[0].error, 'Could not load roles: HTTP 500');
  check('token_form_shows_error_when_roles_load_fails: the menu holds no role',
    outcomes[0].roles, []);
  check('act_as_menu_lists_roles: a loaded list has no error',
    outcomes[1].error, '');
  check('act_as_menu_lists_roles: the menu lists every role, standard first',
    TokenForm.roleOrder(outcomes[1].roles).map((role) => role.name),
    ['planner', 'lead', 'scribe']);
  if (failures) process.exit(1);
});
