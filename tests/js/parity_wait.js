const fs = require('fs');
const os = require('os');
const path = require('path');
const { pathToFileURL } = require('url');
const { check, finish } = require('./support/check.js');

const suite = 'parity_wait: ';
const libUrl = pathToFileURL(path.join(__dirname, '..', '..', '.dev', 'parity', 'lib.mjs')).href;
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'parity-wait-'));
const state = { ftfile: path.join(dir, 'ft.txt'), tz: 'UTC' };

const stubPage = () => {
  const handlers = {};
  const pause = () => new Promise((resolve) => setTimeout(resolve, 1));
  const page = {
    on: (name, fn) => { handlers[name] = fn; },
    waitForTimeout: pause,
    evaluate: () => Promise.resolve(),
    clock: { runFor: pause, fastForward: pause },
  };
  return { page, handlers };
};

const runAdvance = async (runSteps, finishRequest) => {
  const { page, handlers } = stubPage();
  const req = { url: () => 'http://127.0.0.1:1/api/slow' };
  const warnings = [];
  const realWarn = console.warn;
  console.warn = (line) => warnings.push(String(line));
  try {
    const pending = runSteps(page, [{ advance: 1 }], state, 'http://127.0.0.1:1', { clockMs: 0 });
    handlers.request(req);
    if (finishRequest) handlers.requestfinished(req);
    await pending;
  } finally {
    console.warn = realWarn;
  }
  return warnings;
};

const main = async () => {
  const { runSteps } = await import(libUrl);

  const slow = await runAdvance(runSteps, false);
  check(suite + 'a pending request gives one warning', slow.length, 1);
  check(suite + 'the warning names the URL', (slow[0] || '').includes('http://127.0.0.1:1/api/slow'), true);
  check(suite + 'the warning starts with waitForRequests', (slow[0] || '').startsWith('waitForRequests gave up'), true);
  check(suite + 'the warning names the cap', (slow[0] || '').includes('5000 ms'), true);

  const quick = await runAdvance(runSteps, true);
  check(suite + 'a finished request gives no warning', quick.length, 0);

  fs.rmSync(dir, { recursive: true, force: true });
  finish();
};

main().catch((err) => {
  console.log('FAIL ' + suite + String(err && err.stack || err));
  process.exit(1);
});
