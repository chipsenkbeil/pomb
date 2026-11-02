var pages = require('fs').readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
var start = pages.indexOf('  function webhookProbeFailure(');
var stop = pages.indexOf('\n  function wireWebhookSecretDialog');
if (start < 0 || stop < start) throw new Error('webhook probe helpers not found');
eval(pages.slice(start, stop));

var failures = 0;
function check(name, got, want) {
  var ok = got === want;
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got)));
}

check('a status shows as the HTTP code', webhookProbeText({ status: 204, error: null }), 'Answered with HTTP 204');
check('an error shows as the server worded it', webhookProbeText({ status: null, error: 'timed out' }), 'timed out');
check('a refusal shows as the server worded it',
  webhookProbeText({ status: null, error: 'refused: not a public address' }), 'refused: not a public address');
check('a literal private url shows inline as non-public for the DNS check',
  webhookProbeFailure('test-dns', 'url must name a public host, not a loopback, private or link-local address'),
  'Resolves to a non-public address');
check('a literal private url shows inline as refused for the test request',
  webhookProbeFailure('test-request', 'url must name a public host, not a loopback, private or link-local address'),
  'refused: not a public address');
check('another failure keeps the server wording', webhookProbeFailure('test-dns', 'url is not valid'), 'url is not valid');
process.exit(failures ? 1 : 0);
