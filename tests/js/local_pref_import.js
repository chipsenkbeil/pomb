const src = require('fs').readFileSync(process.argv[2], 'utf8');
const start = src.indexOf('  const LEGACY_PREF_KEY');
const end = src.indexOf('  importLocalPrefs();\n', start);
if (start < 0 || end < start) { console.log('FAIL not found: importLocalPrefs'); process.exit(1); }
const body = src.slice(start, end) + '\nimportLocalPrefs();';
const importer = new Function('accountButton', 'accountPrefs', 'window', 'document', 'fetch', body);

let failures = 0;
const check = (name, got, want) => {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
};

const run = (stored, ok) => {
  const data = Object.assign({}, stored);
  const requests = [];
  const classes = {};
  const accountPrefs = { calendarRepeats: true };
  const window = {
    localStorage: {
      getItem: (key) => (key in data ? data[key] : null),
      removeItem: (key) => { delete data[key]; },
    },
  };
  const document = {
    body: { classList: { add: (name) => { classes[name] = true; } } },
    querySelector: () => ({}),
  };
  const fetch = (path, init) => {
    requests.push([path, init.method, JSON.parse(init.body)]);
    return Promise.resolve({ ok });
  };
  importer({}, accountPrefs, window, document, fetch);
  return new Promise((resolve) => {
    setTimeout(() => resolve({ data, requests, classes, accountPrefs }), 10);
  });
};

(async () => {
  let r = await run({ 'pomb-dark': '1', 'pomb-large-names': '1', 'pomb.calendar.settings': '{"showRepeats":false}' }, true);
  check('stored switches are sent once', r.requests, [['/api/settings', 'PUT', { dark: true, large_names: true, calendar_show_repeats: false }]]);
  check('stored switches show at once', [r.classes, r.accountPrefs.calendarRepeats], [{ dark: true, 'large-names': true }, false]);
  check('the old keys go after a saved import', r.data, {});

  r = await run({ 'pomb-dark': '1' }, false);
  check('the old keys stay when the save fails', Object.keys(r.data), ['pomb-dark']);

  r = await run({ 'pomb-dark': '0', 'pomb.calendar.settings': 'not json' }, true);
  check('values with nothing to keep send nothing', r.requests, []);
  check('values with nothing to keep are dropped', r.data, {});

  r = await run({}, true);
  check('no old keys send nothing', r.requests, []);

  if (failures) process.exit(1);
  console.log('ok');
})();
