const src = require('fs').readFileSync(process.argv[2], 'utf8');
const slice = (from, to) => {
  const start = src.indexOf(from);
  const end = src.indexOf(to, start);
  if (start < 0 || end < start) { console.log('FAIL not found: ' + from); process.exit(1); }
  return src.slice(start, end);
};

const root = { innerHTML: '', taskId: null };
const openCalls = [];
let reloads = 0;
const handlers = {};
const location = { pathname: '/b/b1', search: '', hash: '' };
location.reload = () => { reloads++; };
const window = {
  location,
  history: {
    state: null,
    replaceState: (state, title, url) => {
      const at = url.indexOf('#');
      location.hash = at < 0 ? '' : url.slice(at);
    },
  },
};
const document = {
  body: { classList: { remove: () => {} } },
  getElementById: (id) => (id === 'modal-root' ? root : null),
  querySelector: (sel) => {
    if (sel === '#modal-root .task-modal') return root.innerHTML ? { dataset: { taskId: root.taskId } } : null;
    return null;
  },
};
const cssEscape = (s) => s;
const openModal = (taskId) => {
  openCalls.push(taskId);
  root.taskId = taskId;
  root.innerHTML = '<div class="task-modal"></div>';
};
let modalSubviewOpen = false;
let modalTab = 'description';
let modalDirty = false;
eval(slice('  function modalTaskId() {', '  var TASK_HASH'));
eval(slice('  var TASK_HASH', '  function showTaskHash('));
eval(slice('  function clearTaskHash() {', '  function openModal('));
eval(slice('  function closeModal() {', '  function saveModalName() {'));
eval(slice('    function openTaskFromHash() {', "    window.addEventListener('hashchange', openTaskFromHash);"));
handlers.hashchange = openTaskFromHash;

const { check, finish } = require('./support/check.js');

location.hash = '#task-t1';
openTaskFromHash();
check('a task hash opens its card', modalTaskId(), 't1');

location.hash = '#find-x';
handlers.hashchange();
check('back to a find hash closes the modal', root.innerHTML, '');
check('back to a find hash leaves the URL on the finder', location.hash, '#find-x');
check('back to a find hash reads no open card', modalTaskId(), null);

location.hash = '#task-t2';
handlers.hashchange();
check('a hashchange to a task hash opens that card', modalTaskId(), 't2');
check('the opened card is t2', openCalls, ['t1', 't2']);

location.hash = '#layout';
root.innerHTML = '';
handlers.hashchange();
check('an unrelated hash with no modal open does nothing', openCalls, ['t1', 't2']);
check('an unrelated hash with no modal open keeps the URL', location.hash, '#layout');
check('an unrelated hash with no modal open leaves the root empty', root.innerHTML, '');
check('no reload when nothing was open', reloads, 0);

finish();
