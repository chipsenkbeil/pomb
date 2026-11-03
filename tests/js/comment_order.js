var src = require('fs').readFileSync(process.argv[2], 'utf8');
function slice(from, to) {
  var start = src.indexOf(from);
  var end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
}

var stored = {};
var storageBroken = false;
var window = {
  localStorage: {
    getItem: function (k) { if (storageBroken) throw new Error('denied'); return k in stored ? stored[k] : null; },
    setItem: function (k, v) { if (storageBroken) throw new Error('denied'); stored[k] = v; }
  }
};

function node(name) { return { name: name, attrs: {}, getAttribute: function (a) { return this.attrs[a] || null; }, setAttribute: function (a, v) { this.attrs[a] = v; } }; }
var list = node('list');
var composer = node('composer');
var item = node('item');
var section = { children: [list, composer] };
list.parentNode = section;
list.children = ['old', 'mid', 'new'].map(node);
list.appendChild = function (row) {
  list.children = list.children.filter(function (r) { return r !== row; });
  list.children.push(row);
};
Object.defineProperty(list, 'nextSibling', { get: function () { return section.children[section.children.indexOf(list) + 1] || null; } });
section.insertBefore = function (el, ref) {
  if (ref === el) return;
  section.children = section.children.filter(function (c) { return c !== el; });
  var at = ref ? section.children.indexOf(ref) : section.children.length;
  section.children.splice(at, 0, el);
};
var document = {
  getElementById: function (id) { return id === 'modal-comments' ? list : null; },
  querySelector: function (sel) {
    if (sel === '#tm-panel-comments .tm-comment-composer') return composer;
    if (sel === '[data-tm-act="comment-order"]') return item;
    return null;
  }
};
eval(slice("  var COMMENT_ORDER_KEY", "  function wireModalTaskRefs"));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}
function names() { return list.children.map(function (r) { return r.name; }); }
function layout() { return section.children.map(function (c) { return c.name; }); }

check('default is oldest first', loadCommentsNewestFirst(), false);
applyCommentOrder(false);
check('oldest first keeps the composer last', layout(), ['list', 'composer']);
toggleCommentOrder();
check('the toggle reverses the list', names(), ['new', 'mid', 'old']);
check('the toggle moves the composer to the top', layout(), ['composer', 'list']);
check('the item offers the way back', item.textContent, 'Change order - oldest to newest');
check('the choice is stored', stored[COMMENT_ORDER_KEY], '1');

list.children = ['old', 'mid', 'new'].map(node);
list.attrs = {};
section.children = [list, composer];
applyCommentOrder(loadCommentsNewestFirst());
check('a fresh modal reads the choice back', names(), ['new', 'mid', 'old']);
applyCommentOrder(true);
check('applying twice does not flip again', names(), ['new', 'mid', 'old']);
toggleCommentOrder();
check('the second toggle restores the order', names(), ['old', 'mid', 'new']);
check('the second toggle restores the composer', layout(), ['list', 'composer']);
check('the item offers newest first again', item.textContent, 'Change order - newest to oldest');
check('the restored choice is stored', stored[COMMENT_ORDER_KEY], '0');

storageBroken = true;
toggleCommentOrder();
check('a broken storage keeps the choice for the page', loadCommentsNewestFirst(), true);

if (failures) process.exit(1);
console.log('ok');
