var src = require('fs').readFileSync(process.argv[2], 'utf8');
function slice(from, to) {
  var start = src.indexOf(from);
  var end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
}

function Event(type) { this.type = type; }
var modalDirty = false;
var taskId = 't1';
var toasts = [];
var uploads = [];
var uploadOk = true;
function modalTaskId() { return taskId; }
function toast(message) { toasts.push(message); }
function attachmentRefusal() { return Promise.resolve(); }
function hasFiles() { return true; }
var document = { getElementById: function () { return null; } };
function fetch(url) {
  uploads.push(url);
  return Promise.resolve({ ok: uploadOk, json: function () { return Promise.resolve({ id: 'a' + uploads.length, name: 'n' }); } });
}
eval(slice("  var PASTE_IMAGE_TYPE", "  function uploadAttachments"));
eval(slice("  function deleteAttachmentMessage", "  function deleteModalAttachment"));

var failures = 0;
function check(name, got, want) {
  var g = JSON.stringify(got), w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
}

var files = [{ type: 'image/png', name: 'a' }, { type: 'text/plain', name: 'b' }, { type: 'image/svg+xml', name: 'c' }, { type: 'image/webp', name: 'd' }];
check('only the four image types are taken', imageFiles(files).map(function (f) { return f.name; }), ['a', 'd']);
check('no list means no files', imageFiles(null), []);
check('a plain text paste takes nothing', imageFiles([{ type: 'text/plain' }]), []);

check('the alt drops the extension', imageAlt('shot.png'), 'shot');
check('the alt drops brackets and line breaks', imageAlt('a[1]\nb.jpg'), 'a 1 b');
check('an empty name gets a fallback', imageAlt('.png'), 'image');

var fired = [];
var field = { value: 'ab cd', selectionStart: 2, selectionEnd: 3, dispatchEvent: function (e) { fired.push(e.type); } };
insertAtCaret(field, 'XY');
check('the text replaces the selection', field.value, 'abXYcd');
check('the caret follows the text', field.selectionStart, 4);
check('an input event follows', fired, ['input']);

check('no references keeps the plain question', deleteAttachmentMessage(0), 'Delete this attachment?');
check('one reference is named', deleteAttachmentMessage(1).indexOf('1 description or comment shows it') > 0, true);
check('many references are counted', deleteAttachmentMessage(3).indexOf('3 descriptions and comments show it') > 0, true);

function box() {
  var handlers = {};
  return {
    value: '', isConnected: true, closest: function () { return null; }, selectionStart: 0, selectionEnd: 0,
    dispatchEvent: function () {},
    addEventListener: function (type, fn) { handlers[type] = fn; },
    fire: function (type, event) { event.preventDefault = function () { this.prevented = true; }; event.stopPropagation = function () {}; handlers[type](event); return event; },
  };
}
function later() { return new Promise(function (done) { setTimeout(done, 5); }); }
var png = { type: 'image/png', name: 'image.png' };

(async function () {
  var el = box();
  bindImagePaste(el);
  var mixed = el.fire('paste', { clipboardData: { files: [png], types: ['text/plain', 'Files'] } });
  await later();
  check('a clipboard with text is left to the browser', [mixed.prevented, uploads.length], [undefined, 0]);

  var shot = el.fire('paste', { clipboardData: { files: [png], types: ['Files'] } });
  await later();
  check('a lone image is uploaded', [shot.prevented, uploads.length], [true, 1]);
  check('the upload gets a unique name', /name=pasted-\d+-0\.png/.test(uploads[0]), true);
  check('the link lands in the field', el.value.indexOf('![image](attachment:a1)') === 0, true);

  uploadOk = false;
  el.value = '';
  el.fire('paste', { clipboardData: { files: [png], types: ['Files'] } });
  await later();
  check('a refused upload inserts nothing', el.value, '');
  uploadOk = true;

  el.isConnected = false;
  el.fire('paste', { clipboardData: { files: [png], types: ['Files'] } });
  await later();
  check('a gone field inserts nothing and says so', [el.value, toasts[toasts.length - 1].indexOf('was attached') > 0], ['', true]);

  el.isConnected = true;
  toasts = [];
  var pdf = { type: 'application/pdf', name: 'a.pdf' };
  var drop = el.fire('drop', { dataTransfer: { files: [png, pdf] } });
  await later();
  check('a mixed drop takes the image and names the skipped file', [drop.prevented, toasts.length], [true, 1]);

  var plain = el.fire('drop', { dataTransfer: { files: [pdf] } });
  check('a drop without images is left alone', plain.prevented, undefined);

  if (failures) process.exit(1);
  console.log('ok');
})();
