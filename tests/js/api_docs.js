const fs = require('fs');
const src = fs.readFileSync('static/docs.js', 'utf8');
const start = src.indexOf('  const ApiDocs = {');
const end = src.indexOf('\n  };\n', start) + '\n  };\n'.length;
if (start < 0 || end < start) throw new Error('ApiDocs not found');
const ApiDocs = new Function(`${src.slice(start, end)}; return ApiDocs;`)();

let failures = 0;
const check = (name, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${ok ? '' : `: got ${JSON.stringify(got)}`}`);
};

check('a query splits into lowercase terms', ApiDocs.terms('  Claim  TASK '), ['claim', 'task']);
check('a blank query has no terms', ApiDocs.terms('   '), []);
check('every term must appear', ApiDocs.matches('post /api/tasks/{id}/claim', ['claim', 'post']), true);
check('a missing term hides the entry', ApiDocs.matches('get /api/tasks', ['claim']), false);
check('no terms match everything', ApiDocs.matches('anything', []), true);
check('a search names how many entries it shows', ApiDocs.summary(3, 60, ['a']), '3 of 60 entries');
check('a cleared search says nothing', ApiDocs.summary(60, 60, []), '');

if (failures) process.exit(1);
