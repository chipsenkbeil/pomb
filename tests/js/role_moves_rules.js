var source = require('fs').readFileSync(process.argv[2].replace(/app\.js$/, 'pages.js'), 'utf8');
var start = source.indexOf("  pages['board-settings-rules']");
var stop = source.indexOf("\n  pages['board-settings-labels']");
if (start < 0 || stop < start) throw new Error('column rules page not found');

var template = require('fs').readFileSync(process.argv[2].replace(/static\/app\.js$/, 'templates/board_settings.html'), 'utf8');
function fieldsetClass(attribute) {
  var found = template.match(new RegExp('<fieldset class="([^"]*)" ' + attribute + '='));
  if (!found) throw new Error('no fieldset with ' + attribute + ' in the template');
  return found[1];
}

function node(classes, data, props, children) {
  var self = { classes: classes, dataset: data || {}, children: children || [], listeners: {}, hidden: false };
  Object.keys(props || {}).forEach(function (key) { self[key] = props[key]; });
  self.addEventListener = function (type, handler) { self.listeners[type] = handler; };
  self.getAttribute = function (name) { return name === 'data-board-id' ? 'b1' : null; };
  self.matches = function (selector) {
    var parts = selector.split(':');
    if (self.classes.split(' ').indexOf(parts[0].slice(1)) < 0) return false;
    return parts[1] !== 'checked' || self.checked === true;
  };
  self.querySelectorAll = function (selector) {
    var found = [];
    self.children.forEach(function (child) {
      if (child.matches(selector)) found.push(child);
      found = found.concat(child.querySelectorAll(selector));
    });
    return found;
  };
  self.querySelector = function (selector) { return self.querySelectorAll(selector)[0] || null; };
  return self;
}
function box(value) { return node('br-sections', {}, { value: value }); }
function input(cls, value) { return node(cls, {}, { value: value }); }
function check(cls, value, checked) { return node(cls, {}, { value: value, checked: checked }); }

function column(id, sections, min, max, locked, fields) {
  return node(fieldsetClass('data-column-id'), { columnId: id }, {}, [
    box(sections), input('br-min', min), input('br-max', max),
    check('br-outcomes-locked', 'on', locked),
  ].concat(fields));
}
function move(fromId, limited, targets) {
  return node('br-move', { fromId: fromId }, {}, [check('br-limit', 'on', limited)].concat(targets));
}
function role(id, rows) {
  return node(fieldsetClass('data-role-id'), { roleId: id }, {}, rows);
}

var form = node('', {}, {}, [
  column('c1', 'Goal\nPlan', '1', '3', true, [check('br-field', 'due', true), check('br-field', 'owner', false)]),
  column('c2', '', '', '', false, []),
  role('r1', [
    move('c1', true, [check('br-target', 'c2', true), check('br-target', 'c3', false)]),
    move('c2', false, [check('br-target', 'c1', true)]),
  ]),
  role('r2', [move('c1', false, [check('br-target', 'c2', true)])]),
]);

form.requestSubmit = () => { form.listeners.submit({ preventDefault: () => {} }); };
var elements = {
  'br-form': form,
  'br-error': node('', {}, {}),
  'br-saved': node('', {}, {}),
  'br-locked': node('', {}, { value: ' lock-a \n\nlock-b' }),
  'br-standard': node('', {}, {}),
};
var sent = [];
var document = {
  getElementById: function (id) { return elements[id]; },
  querySelector: function () { return node('', {}, {}); },
};
var window = { location: { reload: function () {} } };
var fetch = function (url, options) {
  sent.push({ url: url, options: options });
  return new Promise(function () {});
};
var pages = {};
eval(source.slice(start, stop));
pages['board-settings-rules']();

var failures = 0;
function expect(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  var ok = g === w;
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + g + ' want ' + w));
}

var prevented = 0;
try {
  form.listeners.submit({ preventDefault: function () { prevented++; } });
} catch (error) {
  failures++;
  console.log('FAIL Save rules threw: ' + error.message);
}
expect('Save rules sends one request', sent.length, 1);
if (sent.length) {
  var body = JSON.parse(sent[0].options.body);
  expect('Save rules PUTs the board rules', [sent[0].options.method, sent[0].url], ['PUT', '/api/boards/b1/rules']);
  expect('Save rules holds locked labels, columns and role_moves', Object.keys(body), ['locked_labels', 'columns', 'role_moves']);
  expect('Save rules trims the locked labels', body.locked_labels, ['lock-a', 'lock-b']);
  expect('Save rules holds only the column fieldsets', Object.keys(body.columns), ['c1', 'c2']);
  expect('Save rules sends the first column rules', body.columns.c1,
    { sections: ['Goal', 'Plan'], outcomes_min: 1, outcomes_max: 3, outcomes_locked: true, fields: ['due'] });
  expect('Save rules sends the second column rules', body.columns.c2,
    { sections: [], outcomes_min: null, outcomes_max: null, outcomes_locked: false, fields: [] });
  expect('Save rules sends the limited role moves only', body.role_moves, { r1: { c1: ['c2'] } });
}
expect('Save rules stops the browser submitting the form', prevented, 1);

elements['br-standard'].listeners.click();
expect('Restore standard moves sends one more request', sent.length, 2);
if (sent.length === 2) {
  const restored = JSON.parse(sent[1].options.body);
  expect('Restore standard moves PUTs the board rules', [sent[1].options.method, sent[1].url], ['PUT', '/api/boards/b1/rules']);
  expect('restore_standard_moves_omits_role_moves', Object.keys(restored), ['locked_labels', 'columns']);
  expect('Restore standard moves sends the same columns as the save', restored.columns, JSON.parse(sent[0].options.body).columns);
}

form.listeners.submit({ preventDefault: () => {} });
expect('A plain submit after a restore sends one more request', sent.length, 3);
if (sent.length === 3) {
  expect('A plain submit after a restore still sends role_moves', Object.keys(JSON.parse(sent[2].options.body)), ['locked_labels', 'columns', 'role_moves']);
}

process.exit(failures ? 1 : 0);
