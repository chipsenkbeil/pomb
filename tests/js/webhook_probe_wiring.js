const fs = require('fs');
const src = fs.readFileSync('static/pages.js', 'utf8');
const start = src.indexOf('  function webhookRequest(');
const end = src.indexOf("  pages['board-settings-rules']", start);
if (start < 0 || end < start) throw new Error('webhook form code not found');

const pages = {};
const listeners = {};
const elements = {};
const element = (id) => {
  if (!elements[id]) {
    elements[id] = {
      id,
      value: '',
      disabled: false,
      hidden: true,
      textContent: '',
      dataset: {},
      addEventListener: (type, fn) => { listeners[`${id}:${type}`] = fn; },
    };
  }
  return elements[id];
};
const form = element('wh-form');
form.dataset.boardId = 'b 1';
form.querySelectorAll = () => [];
const document = { getElementById: element };

const sent = [];
const waiting = [];
global.fetch = (url, options) => {
  sent.push({ url, options });
  return new Promise((resolve, reject) => {
    waiting.push({ resolve, reject });
  });
};
const answer = (status, body) => waiting.shift().resolve({
  ok: status >= 200 && status < 300,
  status,
  text: () => Promise.resolve(JSON.stringify(body)),
});
const fail = (message) => waiting.shift().reject(new Error(message));
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

eval(src.slice(start, end));
pages['board-settings-webhook-form']();

element('wh-url').value = '  https://example.com/hook  ';

const { check, finish } = require('./support/check.js');

const probes = [
  { button: 'wh-test-dns', result: 'wh-dns-result', path: 'test-dns' },
  { button: 'wh-test-request', result: 'wh-test-result', path: 'test-request' },
];

const run = async (probe, label, reply, want) => {
  const button = element(probe.button);
  const result = element(probe.result);
  const before = sent.length;
  listeners[`${probe.button}:click`]({ target: button });
  check(`probe_button_should_be_disabled_while_the_request_is_open: ${probe.path} ${label}`, button.disabled, true);
  check(`probe_button_should_show_working_while_the_request_is_open: ${probe.path} ${label}`, result.textContent, 'Working...');
  reply();
  await settle();
  check(`probe_button_should_show_the_answer_of_each_probe: ${probe.path} ${label} text`, result.textContent, want);
  check(`probe_button_should_enable_again_after_a_refusal: ${probe.path} ${label} is enabled again`, button.disabled, false);
  return sent.slice(before);
};

(async () => {
  for (const probe of probes) {
    const posted = await run(probe, 'success', () => {
      if (probe.path === 'test-dns') answer(200, { ok: true, message: 'Resolves to a public address' });
      else answer(200, { status: 204, error: null });
    }, probe.path === 'test-dns' ? 'Resolves to a public address' :'Answered with HTTP 204');
    check(`probe_button_should_show_the_answer_of_each_probe: ${probe.path} posts to its own route`, posted[0].url, `/api/boards/b%201/webhooks/${probe.path}`);
    check(`probe_button_should_show_the_answer_of_each_probe: ${probe.path} posts the trimmed url`, JSON.parse(posted[0].options.body), { url: 'https://example.com/hook' });
    check(`probe_button_should_show_the_answer_of_each_probe: ${probe.path} posts with POST`, posted[0].options.method, 'POST');

    await run(probe, 'refusal', () => answer(400, { message: 'url is not valid' }), 'url is not valid');
    await run(probe, 'network failure', () => fail('Failed to fetch'), 'Failed to fetch');
  }
  finish();
})();
