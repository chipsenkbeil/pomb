var src = require('fs').readFileSync(process.argv[2], 'utf8');
var start = src.indexOf('  var BoardChrome = {');
var end = src.indexOf('\n  };', start) + '\n  };'.length;
if (start < 0 || end < start) throw new Error('BoardChrome not found');

function store() {
  var m = {};
  return {
    getItem: function (k) { return k in m ? m[k] : null; },
    setItem: function (k, v) { m[k] = String(v); },
    removeItem: function (k) { delete m[k]; }
  };
}

function load(sessionStorage, localStorage) {
  var boxes = [];
  var remember = { checked: false };
  var els = {
    'f-user': { tagName: 'SELECT', value: 'all', options: [], dataset: {} },
    'f-color': { tagName: 'SELECT', value: 'all', options: [], dataset: {} },
    'f-date': { tagName: 'SELECT', value: 'all', options: [], dataset: {} },
    'filter-remember': remember,
    'filter-labels-list': {
      appendChild: function (label) { boxes.push(label.input); },
      querySelectorAll: function () { return boxes; }
    }
  };
  var document = {
    getElementById: function (id) { return els[id] || null; },
    querySelector: function () { return null; },
    querySelectorAll: function (sel) {
      return sel === 'input[name="f-label"]:checked' ? boxes.filter(function (b) { return b.checked; }) : [];
    },
    createElement: function (tag) {
      var el = { tag: tag, checked: false };
      el.appendChild = function (child) { if (child.tag === 'input') el.input = child; };
      return el;
    },
    createTextNode: function () { return {}; }
  };
  var window = { sessionStorage: sessionStorage, localStorage: localStorage };
  var boardId = function () { return 'b1'; };
  var chrome = eval('(function () {' + src.slice(start, end) + '\n return BoardChrome; })()');
  chrome.applyFilter = function () {};
  return { chrome: chrome, boxes: boxes, remember: remember, els: els, list: els['filter-labels-list'] };
}

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

var session = store(), local = store();

var page = load(session, local);
page.chrome.addLabelOption(page.list, 'needs:human').checked = true;
page.chrome.maybeSaveFilter();
check('local untouched with remember off', local.getItem('pomb-filter'), null);

page = load(session, local);
page.chrome.restoreFilter();
page.chrome.addLabelOption(page.list, 'other');
page.chrome.restoreLabelFilter();
check('label missing from the board list stays checked', page.chrome.filterLabels(), ['needs:human']);
check('remember stays off', page.remember.checked, false);

page.boxes.forEach(function (b) { b.checked = false; });
page.chrome.maybeSaveFilter();
page = load(session, local);
page.chrome.restoreFilter();
page.chrome.restoreLabelFilter();
check('cleared filter stays cleared', page.chrome.filterLabels(), []);

page = load(session, local);
page.els['f-user'].options = [{ value: 'all', textContent: 'all' }, { value: 'timer', textContent: 'timer' }];
page.els['f-user'].value = 'timer';
page.chrome.maybeSaveFilter();
page = load(session, local);
page.els['f-user'].options = [{ value: 'all', textContent: 'all' }, { value: 'timer', textContent: 'timer' }];
page.chrome.restoreFilter();
check('select filter restored', page.els['f-user'].value, 'timer');

page = load(store(), local);
page.chrome.restoreFilter();
check('new tab starts unfiltered', page.els['f-user'].value, 'all');

var local2 = store();
page = load(store(), local2);
page.remember.checked = true;
page.chrome.addLabelOption(page.list, 'x').checked = true;
page.chrome.maybeSaveFilter();
page = load(store(), local2);
page.chrome.restoreFilter();
page.chrome.restoreLabelFilter();
check('remember restores in a new tab', [page.chrome.filterLabels(), page.remember.checked], [['x'], true]);

if (failures) process.exit(1);
console.log('filter_restore ok');
