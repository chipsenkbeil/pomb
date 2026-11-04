const src = require('fs').readFileSync(process.argv[2], 'utf8');
const slice = (from, to) => {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
};

let posts = 0;
const addModalComment = () => { posts++; };
const { fitCommentBox, onCommentKeydown } = eval('(() => {' + slice('  const fitCommentBox', '  // Comments (KF-064)') + 'return { fitCommentBox, onCommentKeydown };})()');

let failures = 0;
const check = (name, got, want) => {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g !== w) { failures++; console.log('FAIL ' + name + ': got ' + g + ' want ' + w); }
};
const press = (init) => {
  let prevented = false;
  const e = Object.assign({ key: 'Enter', preventDefault: () => { prevented = true; } }, init);
  onCommentKeydown(e);
  return prevented;
};

check('Enter is consumed', press({}), true);
check('Enter posts the comment', posts, 1);

posts = 0;
check('Shift+Enter is not consumed so the box adds a line', press({ shiftKey: true }), false);
check('Shift+Enter does not post', posts, 0);
check('Enter while composing is not consumed', press({ isComposing: true }), false);
check('Enter with keyCode 229 is not consumed', press({ keyCode: 229 }), false);
check('another key is not consumed', press({ key: 'a' }), false);
check('nothing else posted', posts, 0);

const box = { style: { height: '99px' }, scrollHeight: 54, offsetHeight: 54, clientHeight: 54 };
fitCommentBox(box);
check('the box grows to its lines', box.style.height, '54px');
const empty = { style: { height: '99px' }, scrollHeight: 38, offsetHeight: 38, clientHeight: 38 };
fitCommentBox(empty);
check('an empty box keeps its 38px height', empty.style.height, '38px');
const bordered = { style: { height: '99px' }, scrollHeight: 36, offsetHeight: 38, clientHeight: 36 };
fitCommentBox(bordered);
check('a bordered empty box keeps its 38px height', bordered.style.height, '38px');
const wiring = slice('      wireMentions(commentInput);', '    var closeBtn');
check('the composer keydown runs onCommentKeydown', wiring.includes("commentInput.addEventListener('keydown', onCommentKeydown);"), true);
check('the composer grows on input', wiring.includes("commentInput.addEventListener('input', () => fitCommentBox(commentInput));"), true);
check('the composer has no other Enter handler', wiring.includes("e.key === 'Enter'"), false);

const tabBox = { style: { height: '0px' }, scrollHeight: 90, offsetHeight: 90, clientHeight: 90 };
const document = { getElementById: (id) => (id === 'modal-comment-input' ? tabBox : null) };
const { fitOpenCommentBox } = eval('(() => {' + slice('  const fitCommentBox', '  // Comments (KF-064)') + slice('  const fitOpenCommentBox', '  function showModalTab') + 'return { fitOpenCommentBox };})()');
fitOpenCommentBox();
check('opening the Comments tab sizes the box to its lines', tabBox.style.height, '90px');
check('selecting the Comments tab sizes the box', slice('  function selectModalTab', '  const fitOpenCommentBox').includes("if (name === 'comments') fitOpenCommentBox();"), true);

if (failures) process.exit(1);
console.log('ok');
