const { CARD_KIND } = require('../../static/pure.js');
var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var BoardChrome = {');
var end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('BoardChrome not found');

function element(hidden) {
  var classes = {};
  var attrs = {};
  var listeners = {};
  return {
    hidden: hidden,
    style: {},
    classList: {
      toggle: function (name, on) { classes[name] = !!on; },
      contains: function (name) { return !!classes[name]; },
    },
    setAttribute: function (k, v) { attrs[k] = String(v); },
    getAttribute: function (k) { return k in attrs ? attrs[k] : null; },
    addEventListener: function (type, fn) { (listeners[type] = listeners[type] || []).push(fn); },
    dispatchEvent: function (ev) { (listeners[ev.type] || []).forEach(function (fn) { fn(ev); }); },
    click: function () { (listeners.click || []).forEach(function (fn) { fn({ stopPropagation: function () {} }); }); },
  };
}

function load() {
  var els = {
    'filter-btn': element(false),
    'filter-panel': element(true),
    'epics-btn': element(false),
    'epics-panel': element(true),
    'epics-close': element(false),
    'epics-back': element(false),
    'filter-close': element(false),
  };
  var row = { getBoundingClientRect: function () { return { bottom: 80 }; } };
  var docListeners = {};
  var document = {
    getElementById: function (id) { return els[id] || null; },
    querySelector: function (sel) { return sel === '.board-header-row' ? row : null; },
    querySelectorAll: function () { return []; },
    addEventListener: function (type, fn) { (docListeners[type] = docListeners[type] || []).push(fn); },
  };
  var window = {};
  function CustomEvent(type) { this.type = type; }
  var chrome = eval('(function () {' + src.slice(start, end) + '\n return BoardChrome; })()');
  chrome.syncLegendFilter = function () {};
  chrome.isFiltering = function () { return false; };
  chrome.filterLabels = function () { return []; };
  chrome.loadEpics = function () { chrome.loads = (chrome.loads || 0) + 1; };
  chrome.initEpics();
  chrome.initEsc();
  return {
    chrome: chrome,
    els: els,
    escape: function () {
      (docListeners.keydown || []).forEach(function (fn) { fn({ key: 'Escape' }); });
    },
  };
}

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

var page = load();
page.chrome.openFilter();
check('filter opens', page.els['filter-panel'].hidden, false);
page.chrome.openEpics();
check('openEpics closes the filter', [page.els['filter-panel'].hidden, page.els['epics-panel'].hidden], [true, false]);
check('epics button lit and expanded', [
  page.els['epics-btn'].classList.contains('active'),
  page.els['epics-btn'].getAttribute('aria-expanded'),
], [true, 'true']);
check('filter open and epics open each load content', page.chrome.loads, 2);

page.chrome.openFilter();
check('openFilter closes Epics', [page.els['epics-panel'].hidden, page.els['filter-panel'].hidden], [true, false]);
check('epics button unlit and collapsed', [
  page.els['epics-btn'].classList.contains('active'),
  page.els['epics-btn'].getAttribute('aria-expanded'),
], [false, 'false']);

page.els['epics-btn'].click();
check('button opens Epics', page.els['epics-panel'].hidden, false);
check('button open closes the filter', page.els['filter-panel'].hidden, true);
page.els['epics-btn'].click();
check('button closes Epics', [page.els['epics-panel'].hidden, page.els['epics-btn'].classList.contains('active')], [true, false]);

page.chrome.openEpics();
page.escape();
check('Escape closes Epics', [page.els['epics-panel'].hidden, page.els['epics-btn'].classList.contains('active')], [true, false]);

page.chrome.openEpics();
page.els['epics-close'].click();
check('close x closes Epics', [page.els['epics-panel'].hidden, page.els['epics-btn'].getAttribute('aria-expanded')], [true, 'false']);

page.chrome.openEpics();
check('panels dock below the toolbar row', [page.els['epics-panel'].style.top, page.els['filter-panel'].style.top], ['80px', '80px']);

if (failures) process.exit(1);
console.log('epics_toggle ok');
