var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var BoardChrome = {');
var end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('BoardChrome not found');

function card(column, labels) {
  var list = { dataset: { columnId: column } };
  return {
    column: column,
    dataset: { labels: JSON.stringify(labels), colorValue: 'yellow', memberIds: '', taskId: 't' },
    style: { display: '' },
    classList: { toggle: function () {} },
    querySelector: function () { return null; },
    closest: function () { return list; }
  };
}

function header(column, labels) {
  var handlers = {};
  var buttons = labels.map(function (label) {
    var classes = {};
    var count = { textContent: '' };
    return {
      count: count,
      hidden: false,
      querySelector: function () { return count; },
      dataset: { markLabel: label },
      classList: { toggle: function (name, on) { classes[name] = on; } },
      setAttribute: function () {},
      classes: classes
    };
  });
  var clear = {
    hidden: true,
    textContent: '',
    setAttribute: function () {},
    querySelector: function () { return clear; }
  };
  var marks = {
    hidden: false,
    closest: function () { return { dataset: { columnId: column } }; },
    querySelector: function () { return clear; },
    querySelectorAll: function () { return buttons; },
    addEventListener: function (type, fn) { handlers[type] = fn; }
  };
  function click(target) {
    handlers.click({
      target: {
        closest: function (sel) {
          if (sel === '.columnHeader-mark') return target === clear ? null : target;
          return target === clear ? clear : null;
        }
      }
    });
  }
  return { marks: marks, buttons: buttons, clear: clear, click: click };
}

var cards = [
  card('c1', ['needs:human']),
  card('c1', ['other']),
  card('c2', ['other']),
  card('c2', ['needs:human'])
];
var headers = [header('c1', ['needs:human']), header('c2', ['needs:human'])];
var els = {
  'f-user': { tagName: 'SELECT', value: 'all', dataset: {} },
  'f-color': { tagName: 'SELECT', value: 'all', dataset: {} },
  'f-date': { tagName: 'SELECT', value: 'all', dataset: {} }
};
var checkedLabels = [];
var document = {
  getElementById: function (id) { return els[id] || null; },
  querySelector: function () { return null; },
  querySelectorAll: function (sel) {
    if (sel === '.columnHeader-marks') return headers.map(function (h) { return h.marks; });
    if (sel === '.task-card') return cards;
    if (sel === 'input[name="f-label"]:checked') return checkedLabels;
    var inColumn = /^\.task-list\[data-column-id="(\w+)"\] \.task-card$/.exec(sel);
    if (inColumn) return cards.filter(function (c) { return c.column === inColumn[1]; });
    return [];
  }
};
var cssEscape = function (value) { return value; };
var window = {};
var boardId = function () { return 'b1'; };
var chrome = eval('(function () {' + src.slice(start, end) + '\n return BoardChrome; })()');
chrome.refreshFilteredCounts = function () {};
chrome.syncFilterBtn = function () {};
chrome.initColumnLabels();

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function hidden() {
  return cards.map(function (c) { return c.style.display === 'none'; });
}

check('nothing filtered at first', [chrome.isFiltering(), hidden()], [false, [false, false, false, false]]);

headers[0].click(headers[0].buttons[0]);
check('a count narrows only its own column', hidden(), [false, true, false, false]);
check('the filter reports itself', chrome.isFiltering(), true);
check('the clear control shows the label', [headers[0].clear.hidden, headers[0].clear.textContent], [false, 'needs:human']);
check('the other column keeps its clear control hidden', headers[1].clear.hidden, true);

headers[1].click(headers[1].buttons[0]);
check('each column narrows on its own', hidden(), [false, true, true, false]);

headers[0].click(headers[0].clear);
check('clearing one column leaves the other', hidden(), [false, false, true, false]);

headers[1].click(headers[1].buttons[0]);
check('a second click on the count clears it', [hidden(), chrome.isFiltering()], [[false, false, false, false], false]);

checkedLabels = [{ value: 'other' }];
headers[0].click(headers[0].buttons[0]);
check('the column label and the panel label both apply', hidden(), [true, true, false, true]);
checkedLabels = [];
headers[0].click(headers[0].clear);

cards[0].dataset.labels = '[]';
cards[2].dataset.labels = JSON.stringify(['needs:human']);
chrome.refreshMarks();
check('counts follow the cards in each column', headers.map(function (h) { return h.buttons[0].count.textContent; }), ['0', '2']);
check('a chip with no cards hides', headers.map(function (h) { return h.buttons[0].hidden; }), [true, false]);
check('a header with no chips hides its row', headers.map(function (h) { return h.marks.hidden; }), [true, false]);

if (failures) process.exit(1);
console.log('column_label_filter ok');
