const src = require('fs').readFileSync(process.argv[2], 'utf8');
const fn = (start) => {
  const from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
};

const { check, finish } = require('./support/check.js');

let stored = {};
let reloads = 0;
const window = {
  sessionStorage: { setItem: (k, v) => { stored[k] = v; } },
  location: { reload: () => { reloads++; } },
};
eval(fn('  function refusalMessage(res) {'));
eval(fn('  function reloadWithRefusal(res, title) {'));

const response = (body) => ({ ok: false, text: () => Promise.resolve(body) });

(async () => {
  check('a JSON refusal gives its message', await refusalMessage(response('{"message":"column is full"}')), 'column is full');
  check('a plain-text refusal gives its body', await refusalMessage(response('not allowed')), 'not allowed');

  const result = await reloadWithRefusal(response('{"message":"column is full"}'), 'Could not move task.');
  check('a JSON refusal stores the title and message and reloads once', [stored['pomb.pendingToast'], reloads, result],
    ['Could not move task. column is full', 1, null]);

  stored = {};
  reloads = 0;
  await reloadWithRefusal(response('not allowed'), 'Could not move task.');
  check('a plain-text refusal stores the title and message and reloads once', [stored['pomb.pendingToast'], reloads],
    ['Could not move task. not allowed', 1]);

  finish();
})();
