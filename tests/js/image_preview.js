const src = require('fs').readFileSync(process.argv[2], 'utf8');
const start = src.indexOf('  const IMAGE_PREVIEW_MAX');
const end = src.indexOf('  function openDocument(', start);
if (start < 0 || end < start) { console.log('FAIL not found: image preview'); process.exit(1); }

const makeNode = (tag) => {
  const node = {
    tag, className: '', textContent: '', attrs: {}, style: {}, children: [], handlers: {}, removed: false, focused: false,
    setAttribute(name, value) { node.attrs[name] = String(value); },
    getAttribute(name) { return name in node.attrs ? node.attrs[name] : null; },
    appendChild(child) { node.children.push(child); return child; },
    addEventListener(type, fn) { node.handlers[type] = fn; },
    remove() { node.removed = true; },
    focus() { node.focused = true; },
    fire(type, event) {
      const e = Object.assign({ target: node, preventDefault() { this.prevented = true; } }, event);
      node.handlers[type](e);
      return e;
    },
    find(className) {
      const hit = (n) => (' ' + n.className + ' ').includes(' ' + className + ' ');
      const walk = (n) => (hit(n) ? n : n.children.map(walk).find(Boolean));
      return walk(node);
    },
  };
  return node;
};

const body = makeNode('body');
const listeners = [];
const document = {
  body,
  createElement: makeNode,
  addEventListener(type, fn, capture) { listeners.push({ type, fn, capture }); },
  removeEventListener(type, fn) {
    const at = listeners.findIndex((l) => l.type === type && l.fn === fn);
    if (at >= 0) listeners.splice(at, 1);
  },
};
eval(src.slice(start, end) + '\nglobalThis.openImagePreview = openImagePreview;\nglobalThis.imagePreviewTrigger = imagePreviewTrigger;');

let failures = 0;
const check = (name, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ': got ' + JSON.stringify(got) + ' want ' + JSON.stringify(want)));
};

const link = makeNode('a');
link.setAttribute('data-image-preview', '/f/1');
link.setAttribute('data-image-title', 'shot.png');
const target = (hit) => ({ closest: () => hit });

check('non_image_attachment_should_not_open_preview: a plain link opens nothing', imagePreviewTrigger(target(null)), false);
check('non_image_attachment_should_not_open_preview: nothing is mounted', body.children.length, 0);
check('non_image_attachment_should_not_open_preview: no key listener is added', listeners.length, 0);

check('image_attachment_should_open_preview: an image link opens', imagePreviewTrigger(target(link)), true);
const overlay = body.children[0];
const image = overlay.find('image-preview-img');
check('image_attachment_should_open_preview: the overlay is mounted', body.children.length, 1);
check('image_attachment_should_open_preview: the image shows the file', image.getAttribute('src'), '/f/1');
check('image_attachment_should_open_preview: the image starts fitted', image.getAttribute('data-zoom'), 'fit');
check('image_attachment_should_open_preview: the name shows', overlay.find('image-preview-name').textContent, 'shot.png');
check('image_attachment_should_open_preview: the download link points at the file', overlay.find('image-preview-download').getAttribute('href'), '/f/1');
check('image_attachment_should_open_preview: the download link names the file', overlay.find('image-preview-download').getAttribute('download'), 'shot.png');
check('image_attachment_should_open_preview: the dialog is labelled', overlay.getAttribute('role'), 'dialog');
check('image_attachment_should_open_preview: Escape is caught first', listeners.map((l) => [l.type, l.capture]), [['keydown', true]]);

const key = listeners[0].fn;
const other = { key: 'a', preventDefault() {}, stopImmediatePropagation() {} };
key(other);
check('image_attachment_should_open_preview: another key keeps it open', overlay.removed, false);
const esc = { key: 'Escape', preventDefault() { this.prevented = true; }, stopImmediatePropagation() { this.stopped = true; } };
key(esc);
check('image_attachment_should_open_preview: Escape closes it', overlay.removed, true);
check('image_attachment_should_open_preview: Escape does not reach the card', [esc.prevented, esc.stopped], [true, true]);
check('image_attachment_should_open_preview: the key listener goes', listeners.length, 0);

const scaleOf = (img) => Number(/scale\(([\d.]+)\)/.exec(img.style.transform)[1]);
const open = () => {
  const o = openImagePreview('/f/2', 'b.png', '/d/2');
  return { o, img: o.find('image-preview-img'), buttons: o.children[0].children.filter((n) => n.tag === 'button') };
};
let view = open();
const [zoomOut, zoomIn] = view.buttons;
check('image_preview_should_zoom: it starts at the fitted size', scaleOf(view.img), 1);
zoomOut.fire('click', {});
check('image_preview_should_zoom: zooming out at the fitted size stays fitted', scaleOf(view.img), 1);
zoomIn.fire('click', {});
const once = scaleOf(view.img);
check('image_preview_should_zoom: zooming in enlarges the image', once > 1, true);
check('image_preview_should_zoom: the image reports it is zoomed', view.img.getAttribute('data-zoom'), 'in');
zoomIn.fire('click', {});
check('image_preview_should_zoom: zooming in again enlarges it more', scaleOf(view.img) > once, true);
for (let i = 0; i < 40; i++) zoomIn.fire('click', {});
check('image_preview_should_zoom: zoom stops at the limit', scaleOf(view.img), 8);
view.img.fire('pointerdown', { pointerId: 1, clientX: 10, clientY: 10 });
view.img.fire('pointermove', { pointerId: 1, clientX: 25, clientY: 4 });
view.img.fire('pointerup', { pointerId: 1 });
check('image_preview_should_zoom: a zoomed image pans', view.img.style.transform.startsWith('translate(15px, -6px)'), true);
for (let i = 0; i < 40; i++) zoomOut.fire('click', {});
check('image_preview_should_zoom: zooming out returns to the fitted size', view.img.style.transform, 'translate(0px, 0px) scale(1)');
check('image_preview_should_zoom: the image reports it is fitted again', view.img.getAttribute('data-zoom'), 'fit');
const stage = view.o.find('image-preview-stage');
stage.fire('wheel', { deltaY: -100 });
check('image_preview_should_zoom: the wheel up zooms in', scaleOf(view.img) > 1, true);
stage.fire('wheel', { deltaY: 100 });
check('image_preview_should_zoom: the wheel down zooms out', scaleOf(view.img), 1);

view.img.fire('pointerdown', { pointerId: 1, clientX: 0, clientY: 0 });
view.img.fire('pointerdown', { pointerId: 2, clientX: 100, clientY: 0 });
view.img.fire('pointermove', { pointerId: 2, clientX: 200, clientY: 0 });
check('image_preview_should_zoom: a pinch spreading zooms in', scaleOf(view.img), 2);
view.img.fire('pointerup', { pointerId: 1 });
view.img.fire('pointerup', { pointerId: 2 });

view.o.fire('click', { target: view.img });
check('image_attachment_should_open_preview: a click on the image keeps it open', view.o.removed, false);
view.o.fire('click', { target: view.o });
check('image_attachment_should_open_preview: a click outside closes it', view.o.removed, true);

view = open();
view.buttons[2].fire('click', {});
check('image_attachment_should_open_preview: the close button closes it', view.o.removed, true);
const first = open();
const second = open();
check('image_attachment_should_open_preview: opening again replaces the first', [first.o.removed, second.o.removed, listeners.length], [true, false, 1]);
const css = require('fs').readFileSync(require('path').join(require('path').dirname(process.argv[2]), 'style.css'), 'utf8');
const rule = /\.tm-md img \{([^}]*)\}/.exec(css);
check('comment_image_should_fit_width: the rule exists', rule !== null, true);
check('comment_image_should_fit_width: width is capped', /max-width:\s*100%/.test(rule[1]), true);
check('comment_image_should_fit_width: height follows', /height:\s*auto/.test(rule[1]), true);
check('comment_image_should_fit_width: the cursor zooms', /cursor:\s*zoom-in/.test(rule[1]), true);

const commentImage = (src, alt, linked) => {
  const img = makeNode('img');
  if (src !== null) img.setAttribute('src', src);
  if (alt) img.setAttribute('alt', alt);
  img.closest = (sel) => (sel === '.tm-md img' ? img : sel === 'a' && linked ? makeNode('a') : null);
  return { closest: (sel) => (sel === '[data-image-preview]' ? null : img.closest(sel)) };
};
while (body.children.length) body.children.pop();
check('comment_image_should_open_preview: an image opens', imagePreviewTrigger(commentImage('/files/shot.png?v=1', 'a screenshot', false)), true);
const commentOverlay = body.children[body.children.length - 1];
check('comment_image_should_open_preview: the preview shows the source', commentOverlay.find('image-preview-img').getAttribute('src'), '/files/shot.png?v=1');
check('comment_image_should_open_preview: the alt text is the title', commentOverlay.find('image-preview-name').textContent, 'a screenshot');
check('comment_image_should_open_preview: the source is the download link', commentOverlay.find('image-preview-download').getAttribute('href'), '/files/shot.png?v=1');
imagePreviewTrigger(commentImage('/files/shot.png?v=1', '', false));
check('comment_image_should_open_preview: no alt falls back to the file name', body.children[body.children.length - 1].find('image-preview-name').textContent, 'shot.png');
const before = body.children.length;
check('linked_comment_image_should_follow_link: a linked image opens nothing', imagePreviewTrigger(commentImage('/files/shot.png', 'x', true)), false);
check('linked_comment_image_should_follow_link: nothing is mounted', body.children.length, before);
process.exit(failures ? 1 : 0);
