var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var BoardChrome = {');
var end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('BoardChrome not found');
var wipStart = src.indexOf('  function wipLimitState(');
var wipEnd = src.indexOf('\n  }', wipStart) + '\n  }'.length;
if (wipStart < 0 || wipEnd < wipStart) throw new Error('wipLimitState not found');

function select(value, key) {
  return { tagName: 'SELECT', value: value, options: [], dataset: { fieldKey: key } };
}

function card(id, labels, fields) {
  return {
    dataset: { taskId: id, labels: JSON.stringify(labels), filterFields: JSON.stringify(fields), memberIds: '', dueAt: '' },
    style: {},
    classList: { toggle: function () {} },
    querySelector: function () { return null; },
    closest: function () { return null; },
  };
}

function load() {
  var cards = [
    card('1', ['a'], { stage: 'Draft' }),
    card('2', ['a', 'b'], { stage: 'Live' }),
    card('3', ['b'], { stage: 'Live' }),
    card('4', [], {}),
    card('5', [], { stage: 'all' }),
  ];
  var boxes = ['a', 'b'].map(function (value) { return { value: value, checked: false }; });
  var stage = select('', 'stage');
  stage.options = [{ value: '' }, { value: 'Draft' }, { value: 'Live' }, { value: 'all' }];
  var mode = select('and');
  var count = { textContent: '' };
  var header = {
    dataset: { columnId: 'c1', wipLimit: '1', taskCount: '4' },
    classList: { toggle: function () {} },
    querySelector: function (sel) { return sel === '.columnHeader-count' ? count : null; },
  };
  var button = {
    classes: {},
    classList: { toggle: function (name, on) { button.classes[name] = on; } },
  };
  var reset = { hidden: true };
  var panelReset = { hidden: true };
  var panel = { hidden: true };
  var els = {
    'f-user': select('all'),
    'f-color': select('all'),
    'f-date': select('all'),
    'f-label-mode': mode,
    'filter-btn': button,
    'filter-reset': reset,
    'filter-reset-panel': panelReset,
    'filter-panel': panel,
  };
  var document = {
    getElementById: function (id) { return els[id] || null; },
    querySelector: function () { return null; },
    querySelectorAll: function (sel) {
      if (sel === '.task-card') return cards;
      if (sel === '.f-field') return [stage];
      if (sel === 'input[name="f-label"]:checked') return boxes.filter(function (b) { return b.checked; });
      if (sel === 'input[name="f-label"]') return boxes;
      if (sel === '.columnHeader[data-column-id]') return [header];
      if (sel.indexOf('.task-list[data-column-id=') === 0) return cards;
      return [];
    },
  };
  var window = { sessionStorage: null, localStorage: null };
  var boardId = function () { return 'b1'; };
  var preamble = 'var cssEscape = function (s) { return s; }; var cardEstimateMinutes = function () { return 0; };' +
    'var setSumBadge = function () {}; var syncWarningLine = function () {}; var toast = function () {};' +
    src.slice(wipStart, wipEnd);
  var chrome = eval('(function () {' + preamble + src.slice(start, end) + '\n return BoardChrome; })()');
  return { chrome: chrome, cards: cards, boxes: boxes, stage: stage, mode: mode, count: count, button: button, reset: reset, panelReset: panelReset, panel: panel };
}

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function shown(page) {
  return page.cards.filter(function (c) { return c.style.display !== 'none'; }).map(function (c) { return c.dataset.taskId; });
}

var page = load();
page.boxes[0].checked = true;
page.boxes[1].checked = true;
page.chrome.applyFilter();
check('and is the default and needs every label', shown(page), ['2']);

page.mode.value = 'or';
page.chrome.applyFilter();
check('or shows a card with any chosen label', shown(page), ['1', '2', '3']);

page.mode.value = 'and';
page.chrome.applyFilter();
check('and shows only cards with all of them', shown(page), ['2']);

page = load();
page.stage.value = 'Live';
page.chrome.applyFilter();
check('a dropdown field keeps the cards holding its option', shown(page), ['2', '3']);
check('the funnel turns on', page.button.classes.filtering, true);
check('the reset link shows', page.reset.hidden, false);
check('a filtered header drops the limit', page.count.textContent, '2');

page.stage.value = 'all';
page.chrome.applyFilter();
check('an option named all filters like any other', shown(page), ['5']);
page.stage.value = 'Live';
page.boxes[0].checked = true;
page.chrome.applyFilter();
check('field and label filters combine', shown(page), ['2']);

page.panel.hidden = false;
page.chrome.syncFilterBtn();
check('the open panel hides the link beside the funnel', page.reset.hidden, true);
check('the open panel shows its own link', page.panelReset.hidden, false);
page.panel.hidden = true;
page.chrome.syncFilterBtn();
check('the closed panel shows the link beside the funnel', page.reset.hidden, false);
check('the closed panel hides its own link', page.panelReset.hidden, true);

page.chrome.resetFilter();
check('reset shows every card', shown(page), ['1', '2', '3', '4', '5']);
check('reset clears the field select', page.stage.value, '');
check('reset clears the labels', page.chrome.filterLabels(), []);
check('reset turns the funnel off', page.button.classes.filtering, false);
check('reset hides the link', page.reset.hidden, true);
check('reset hides the panel link', page.panelReset.hidden, true);
check('an unfiltered header keeps the limit', page.count.textContent, '4 / 1');

page = load();
page.mode.value = 'or';
page.stage.value = 'Draft';
var state = { labelMode: page.chrome.filterLabelMode(), fields: page.chrome.filterFields() };
page = load();
page.chrome.setLabelMode(state.labelMode);
page.chrome.setFilterFields(state.fields);
check('the label mode round trips', page.mode.value, 'or');
check('the field choice round trips', page.stage.value, 'Draft');

if (failures) process.exit(1);
console.log('filter modes ok');
