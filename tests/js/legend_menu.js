var src = require('fs').readFileSync(process.argv[2], 'utf8');
function slice(from, to) {
  var start = src.indexOf(from);
  var end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
}

var toasts = [];
var requests = [];
var reloads = 0;
var prefs = {};
var classes = {};
var attrs = {};
var footer = { hidden: true };
var segments = [];
var select = { value: 'all', dispatched: [], dispatchEvent: function (ev) { this.dispatched.push(ev.type); } };
var apiBody = { legend_visible: true };
function Event(type) { this.type = type; }
function toast(msg) { toasts.push(msg); }
function boardId() { return 'b1'; }
function api(path, method, data) {
  requests.push([method, path, data]);
  return Promise.resolve({ ok: true, json: function () { return Promise.resolve(apiBody); } });
}
function makeItem(name) {
  return { setAttribute: function (k, v) { attrs[name] = v; }, getAttribute: function () { return attrs[name]; } };
}
var document = {
  body: { classList: { toggle: function (n, on) { classes[n] = on; }, contains: function (n) { return !!classes[n]; } } },
  querySelector: function (sel) {
    var m = /data-bm="([^"]*)"/.exec(sel);
    if (m) return makeItem(m[1]);
    return sel === '.color-legend' ? footer : null;
  },
  querySelectorAll: function () { return segments; },
  getElementById: function (id) { return id === 'f-color' ? select : null; }
};
var window = { location: { reload: function () { reloads++; } } };
var MENU_KEEPS_OPEN;
eval(slice('  var MENU_KEEPS_OPEN =', '  var BoardChrome ='));
var chrome = {
  setPref: function (k, on) { prefs[k] = on; },
  filterValue: function () { return select.value; }
};
eval('Object.assign(chrome, {' + slice('    toggleDark: function', '    // KF-379: the help dialog').replace(/,\s*$/, '') + '});');
eval('Object.assign(chrome, {' + slice('    syncLegendFilter: function', '    initLegend: function').replace(/,\s*$/, '') + '});');

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

check('the three toggles keep the Menu open', [MENU_KEEPS_OPEN.dark, MENU_KEEPS_OPEN.legend, MENU_KEEPS_OPEN['large-names'], MENU_KEEPS_OPEN.help], [true, true, true, undefined]);

chrome.toggleDark();
chrome.toggleLargeNames();
check('dark mode and large names show no toast', toasts, []);
check('dark mode and large names still switch', [classes.dark, classes['large-names'], attrs.dark, attrs['large-names']], [true, true, 'true', 'true']);

function segment(value) {
  var on = false;
  var s = { dataset: { colorValue: value }, classList: { toggle: function (n, flag) { on = flag; } } };
  s.bold = function () { return on; };
  segments.push(s);
  return s;
}
var blue = segment('blue'), red = segment('red');
select.value = 'red';
chrome.syncLegendFilter();
check('the filtered color turns bold', [blue.bold(), red.bold()], [false, true]);
select.value = 'all';
chrome.syncLegendFilter();
check('no filter leaves every segment plain', [blue.bold(), red.bold()], [false, false]);

check('Filter on color sets the color filter', [chrome.filterOnColor('blue'), select.value, select.dispatched], [true, 'blue', ['change']]);

attrs.legend = 'false';
chrome.toggleLegend();
setTimeout(function () {
  check('the legend toggle shows the footer without a reload or a toast', [footer.hidden, reloads, toasts, requests[0][2]], [false, 0, [], { legend_visible: true }]);
  if (failures) process.exit(1);
  console.log('ok');
}, 20);
