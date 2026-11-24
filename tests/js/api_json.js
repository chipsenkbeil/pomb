const src = require('fs').readFileSync(process.argv[2], 'utf8');
const slice = (from, to) => {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
};

const calls = [];
const fetch = (path, init) => { calls.push({ path, init }); return Promise.resolve({ ok: true }); };
const apiJson = eval(slice("  function api(path", "  // read the response body") + "\napiJson;");

const { check, finish } = require('./support/check.js');
const suite = 'api_json: ';

apiJson('/api/tasks', 'POST', { name: 'Card' });
const posted = calls[0];
check(suite + 'the path is kept', posted.path, '/api/tasks');
check(suite + 'the method is kept', posted.init.method, 'POST');
check(suite + 'Accept asks for JSON', posted.init.headers.Accept, 'application/json');
check(suite + 'Content-Type is JSON', posted.init.headers['Content-Type'], 'application/json');
check(suite + 'the body is stringified', posted.init.body, '{"name":"Card"}');

apiJson('/api/tasks/1', 'DELETE');
check(suite + 'no data sends no body', calls[1].init.body, undefined);

apiJson('/api/tasks', 'POST', { a: 1 }, { 'X-Extra': 'yes' });
check(suite + 'extra headers are added', calls[2].init.headers['X-Extra'], 'yes');
check(suite + 'extra headers keep Accept', calls[2].init.headers.Accept, 'application/json');

const bodyOf = (name) => {
  const start = src.indexOf('  function ' + name + '(');
  const end = src.indexOf('\n  }\n', start);
  return src.slice(start, end);
};
['createNewChild', 'postEpic', 'epicRequest'].forEach((name) => {
  const text = bodyOf(name);
  check(suite + name + ' calls apiJson', /\bapiJson\(/.test(text), true);
  check(suite + name + ' no longer calls fetch', /\bfetch\(/.test(text), false);
});

const groupOne = [
  "apiJson('/api/boards/' + encodeURIComponent(board) + '/export-data', 'GET')",
  "sidebarFoldLoad = apiJson('/api/settings', 'GET')",
  "apiJson('/api/tasks/' + encodeURIComponent(id), 'PATCH', patch)",
  "apiJson('/api/boards/' + encodeURIComponent(board) + '/epics', 'GET')",
  "apiJson('/api/settings', 'GET')\n        .then(function (r) { if (!r.ok) throw new Error('HTTP ' + r.status); return r.json(); })\n        .then(function (s) {\n          self.settings = s;",
  "return apiJson(url, 'GET')\n        .then(function (res) { return jsonIfJson(res); })\n        .then(function (body) {\n          if (!body || !body.items)",
  "apiJson('/api/timer/settings', 'GET')",
  "return apiJson('/api/timer/status', 'GET')",
  "apiJson('/api/timer/today', 'GET')",
  "apiJson('/api/timer/status', 'GET')\n          .then(function (res) { return jsonIfJson(res); })\n          .then(function (status) {\n            var count",
];
groupOne.forEach((call) => {
  const at = src.indexOf(call);
  const name = suite + 'api_json_should_move_group_one ' + call.split('\n')[0];
  check(name + ' uses apiJson', at >= 0, true);
  const before = src.slice(Math.max(0, at - 40), at + call.indexOf('apiJson'));
  check(name + ' has no fetch', /\bfetch\(/.test(before), false);
});

finish();
