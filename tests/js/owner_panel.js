const src = require('fs').readFileSync(process.argv[2], 'utf8');
function fn(start) {
  const from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
}

let failures = 0;
function check(name, got, want) {
  const g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

function flush() {
  return new Promise((resolve) => { setImmediate(resolve); });
}

function response(ok, body) {
  return { ok: ok, text: () => Promise.resolve(body) };
}

let calls, confirms, refreshes, replies, boxes, modalDirty, fields;

function reset() {
  calls = [];
  confirms = [];
  refreshes = 0;
  replies = [];
  boxes = {};
  modalDirty = false;
  fields = { 'tm-seat-name': { value: '  docs  ' }, 'tm-seat-role': { value: 'reviewer' } };
}

const document = {
  getElementById: function (id) {
    if (fields[id]) return fields[id];
    if (!boxes[id]) boxes[id] = { textContent: '', hidden: true };
    return boxes[id];
  },
};
function modalTaskId() { return 't 1'; }
function refreshModal() { refreshes++; }
function errorMessage(text) {
  try { return JSON.parse(text).message || text; } catch (e) { return text; }
}
function fetch(path, init) {
  calls.push({ path: path, method: init.method, headers: init.headers, body: init.body });
  return Promise.resolve(replies.length ? replies.shift() : response(true, ''));
}
function showConfirmDialog(title, message, okLabel, onConfirm) {
  confirms.push({ title: title, okLabel: okLabel, ok: onConfirm });
}
eval(fn('  function showOwnerPanelError(boxId, message) {'));
eval(fn('  function ownerPanelSend(spec) {'));
eval(fn('  function ownerPanelSpec(button) {'));
eval(fn('  function runOwnerPanelAction(button) {'));

function button(attrs) {
  return { getAttribute: (name) => name in attrs ? attrs[name] : null };
}

async function main() {
  reset();
  await runOwnerPanelAction(button({ 'data-owner-act': 'owner-take', 'data-over': '1', 'data-live-agent': '1' }));
  check('take_over_should_ask_once_only_for_a_live_agent_lease', confirms.length, 1);
  check('take_over_should_send_nothing_before_the_answer', calls.length, 0);
  confirms[0].ok();
  await flush();
  check('take_over_should_send_after_the_answer', calls.map((c) => [c.path, c.method, c.body]),
    [['/api/tasks/t%201/owner', 'POST', '{"take_over":true}']]);
  check('take_over_should_refresh_the_modal', [refreshes, modalDirty], [1, true]);

  reset();
  await runOwnerPanelAction(button({ 'data-owner-act': 'owner-take', 'data-over': '1' }));
  check('take_over_should_not_ask_without_a_live_agent_lease', [confirms.length, calls.length], [0, 1]);
  await runOwnerPanelAction(button({ 'data-owner-act': 'seat-take', 'data-over': '1', 'data-live-agent': '1', 'data-seat': 'code' }));
  check('take_over_should_ask_once_for_a_live_agent_seat', [confirms.length, calls.length], [1, 1]);

  reset();
  await runOwnerPanelAction(button({ 'data-owner-act': 'ask-again' }));
  check('panel_should_call_the_seat_api_from_ask_again', calls.map((c) => [c.path, c.method, c.body]),
    [['/api/tasks/t%201/seats/ask-again', 'POST', '{}']]);

  reset();
  await runOwnerPanelAction(button({ 'data-owner-act': 'owner-take', 'data-ttl': '14400' }));
  await runOwnerPanelAction(button({ 'data-owner-act': 'owner-take' }));
  await runOwnerPanelAction(button({ 'data-owner-act': 'release', 'data-fence': '7' }));
  check('panel_should_send_the_ttl_of_a_timed_take', [calls[0].body, calls[1].body],
    ['{"take_over":false,"ttl_seconds":14400}', '{"take_over":false}']);
  check('panel_should_send_the_fence_with_a_release', [calls[2].path, calls[2].headers['If-Owner-Fence']],
    ['/api/tasks/t%201/owner/release', '7']);

  reset();
  await runOwnerPanelAction(button({ 'data-owner-act': 'seat-add' }));
  await runOwnerPanelAction(button({ 'data-owner-act': 'seat-drop', 'data-seat': 'code review' }));
  check('panel_should_add_and_drop_a_seat_on_the_card', calls.map((c) => [c.path, c.method, c.body]),
    [['/api/tasks/t%201/seats', 'POST', '{"name":"docs","role":"reviewer"}'],
     ['/api/tasks/t%201/seats?name=code%20review', 'DELETE', undefined]]);

  reset();
  replies.push(response(false, '{"message":"seat held"}'));
  await runOwnerPanelAction(button({ 'data-owner-act': 'seat-take', 'data-seat': 'code' }));
  check('panel_should_show_the_refusal_of_a_seat_take', [boxes['tm-seat-error'].textContent, boxes['tm-seat-error'].hidden, refreshes],
    ['seat held', false, 0]);

  console.log('failures=' + failures);
  if (failures) throw new Error(failures + ' owner panel check(s) failed');
}
main();
