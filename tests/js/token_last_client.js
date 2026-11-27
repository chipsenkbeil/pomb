const fs = require('fs');
const src = fs.readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
const formStart = src.indexOf('  var TokenForm = {');
const formEnd = src.indexOf('\n  };\n', formStart) + '\n  };\n'.length;
const start = src.indexOf('  const TokenInspect = {');
const stop = src.indexOf('\n  function wireTokenCreatedDialog');
const sortStart = src.indexOf('  var TokenSort = {');
if (formStart < 0 || formEnd < formStart) throw new Error('TokenForm not found');
if (start < 0 || stop < start) throw new Error('TokenInspect not found');
if (sortStart < 0 || sortStart > start) throw new Error('TokenSort not found');
eval(src.slice(formStart, formEnd));
const loaded = eval(src.slice(sortStart, stop) + '\n({ sort: TokenSort, inspect: TokenInspect })');
const inspect = loaded.inspect;
const sort = loaded.sort;

const { check, finish } = require('./support/check.js');

const elements = {};
const doc = {
  getElementById: (id) => {
    if (!elements[id]) elements[id] = { id: id, textContent: '', children: [], appendChild(child) { this.children.push(child); } };
    return elements[id];
  },
  createElement: (tag) => ({ tagName: tag, textContent: '' }),
};

const base = { name: 'CI bot', expires_at: null, boards: 'all', permissions: ['tasks.read'] };

inspect.fill(doc, Object.assign({}, base, {
  last_used_at: 1767225600,
  last_used_network: '203.0.113.7',
  last_used_agent: 'curl/8.4.0',
}));
check('last_client_rows: a used token shows its network and user agent',
  elements['token-inspect-last-client'].textContent, 'Last client: 203.0.113.7 - curl/8.4.0');
check('last_client_rows: the expiry still shows beside it',
  elements['token-inspect-expiry'].textContent, 'never expires');

inspect.fill(doc, Object.assign({}, base, {
  last_used_at: null,
  last_used_network: null,
  last_used_agent: null,
}));
check('last_client_rows: a token never used shows never',
  elements['token-inspect-last-client'].textContent, 'Last client: never');

inspect.fill(doc, Object.assign({}, base, {
  last_used_at: 1767225600,
  last_used_network: null,
  last_used_agent: null,
}));
check('last_client_rows: a token used before recording shows unknown',
  elements['token-inspect-last-client'].textContent, 'Last client: unknown');

check('last_client_rows: the sort columns follow the table cells',
  sort.columns.indexOf('last_client'), 5);

finish();
