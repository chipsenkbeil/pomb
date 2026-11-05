var pages = require('fs').readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
var start = pages.indexOf('  function tokenRequest(');
var stop = pages.indexOf("\n  pages['board-settings-webhooks']");
if (start < 0 || stop < start) throw new Error('token row helpers not found');

var elements = {};
function element(id) {
  if (!elements[id]) elements[id] = { id: id, hidden: false, value: '', textContent: '', listeners: {} };
  elements[id].addEventListener = function (type, handler) { elements[id].listeners[type] = handler; };
  return elements[id];
}
var document = {
  getElementById: function (id) { return element(id); },
  querySelectorAll: function () { return []; }
};
var sent = [];
var window = {
  pombPasskey: {
    fetch: function (url, options) {
      sent.push({ url: url, options: options });
      return new Promise(function () {});
    }
  },
  location: { reload: function () {} }
};
eval(pages.slice(start, stop));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  var ok = g === w;
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + g + ' want ' + w));
}

element('token-table');
wireTokenRows();
var submit = elements['tk-edit-form'].listeners.submit;
var prevented = 0;
function press() { submit({ preventDefault: function () { prevented++; } }); }

element('tk-edit-name').value = '  Renamed  ';
element('tk-edit-child-hours').value = '4';
press();
check('the edit submit sends one PATCH', sent.length, 1);
check('the edit submit holds only name and child_lifetime_hours',
  Object.keys(JSON.parse(sent[0].options.body)).sort(), ['child_lifetime_hours', 'name']);
check('the edit submit trims the name and sends the hours as a number',
  JSON.parse(sent[0].options.body), { name: 'Renamed', child_lifetime_hours: 4 });
check('the edit submit stops the browser submitting the form', prevented, 1);

element('tk-edit-name').value = '   ';
press();
check('a blank name sends nothing', sent.length, 1);
check('a blank name shows an error', elements['tk-edit-error'].textContent, 'Give the token a name.');

process.exit(failures ? 1 : 0);
