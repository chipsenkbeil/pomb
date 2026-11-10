var LocalTime = (function () {
  let prefs = { hour12: true };
  var SHORT_MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  var LONG_MONTHS = ['January', 'February', 'March', 'April', 'May', 'June', 'July', 'August',
    'September', 'October', 'November', 'December'];

  function parts(ms, zone) {
    var found = {};
    new Intl.DateTimeFormat('en-US', {
      timeZone: zone, hourCycle: 'h23', year: 'numeric', month: 'numeric', day: 'numeric',
      hour: 'numeric', minute: 'numeric', weekday: 'long',
    }).formatToParts(new Date(ms)).forEach(function (part) { found[part.type] = part.value; });
    var year = +found.year;
    var month = +found.month;
    var day = +found.day;
    return {
      year: year, month: month, day: day, hour: +found.hour % 24, minute: +found.minute,
      weekday: found.weekday, dayNo: Math.floor(Date.UTC(year, month - 1, day) / 86400000),
    };
  }

  function clock(p, hour12) {
    var minutes = (p.minute < 10 ? '0' : '') + p.minute;
    if (!hour12) return (p.hour < 10 ? '0' : '') + p.hour + ':' + minutes;
    return (p.hour % 12 || 12) + ':' + minutes + ' ' + (p.hour < 12 ? 'AM' : 'PM');
  }

  function dueDay(p, diff, ms, now) {
    if (diff === 0) return 'Today';
    return ms > now && diff < 7 ? p.weekday : null;
  }

  function write(shape, ms, now, zone, hour12) {
    var p = parts(ms, zone);
    var diff = p.dayNo - parts(now, zone).dayNo;
    var c = clock(p, hour12);
    var label;
    switch (shape) {
      case 'datetime':
        return SHORT_MONTHS[p.month - 1] + ' ' + (p.day < 10 ? '0' : '') + p.day + ', ' + p.year + ' ' + c;
      case 'comment':
        if (diff === 0) return 'Today ' + c;
        if (diff === -1) return 'Yesterday ' + c;
        return SHORT_MONTHS[p.month - 1] + ' ' + p.day + ' ' + c;
      case 'due-card':
        label = dueDay(p, diff, ms, now);
        return label ? label + ' ' + c : p.day + ' ' + LONG_MONTHS[p.month - 1] + ' ' + c;
      case 'due-modal':
        label = dueDay(p, diff, ms, now);
        return label ? label + ' ' + c : SHORT_MONTHS[p.month - 1] + ' ' + p.day + ' at ' + c;
      default:
        return c;
    }
  }

  function writeSpan(startMs, endMs, now, zone, hour12) {
    return write('clock', startMs, now, zone, hour12) + ' - ' + write('clock', endMs, now, zone, hour12);
  }

  function wrote(el) {
    var at = Date.parse(el.getAttribute('data-at'));
    if (isNaN(at)) return null;
    var choice = el.getAttribute('data-clock');
    var hour12 = choice ? choice !== '24h' : prefs.hour12;
    var now = Date.now();
    if (el.getAttribute('data-fmt') === 'range') {
      var end = Date.parse(el.getAttribute('data-end'));
      return isNaN(end) ? null : writeSpan(at, end, now, undefined, hour12);
    }
    return write(el.getAttribute('data-fmt'), at, now, undefined, hour12);
  }

  function apply(root) {
    var found = Array.prototype.slice.call(root.querySelectorAll('[data-at]:not([data-local])'));
    if (root.matches && root.matches('[data-at]:not([data-local])')) found.push(root);
    found.forEach(function (el) {
      var text = wrote(el);
      if (text != null) {
        var attr = el.getAttribute('data-attr');
        if (attr) el.setAttribute(attr, text); else el.textContent = text;
      }
      el.setAttribute('data-local', '');
    });
  }

  function watch() {
    apply(document);
    new MutationObserver(function (records) {
      records.forEach(function (record) {
        Array.prototype.forEach.call(record.addedNodes, function (node) {
          if (node.nodeType === 1) apply(node);
        });
      });
    }).observe(document.documentElement, { childList: true, subtree: true });
  }

  function span(startIso, endIso, fallback) {
    var start = Date.parse(startIso);
    var end = Date.parse(endIso);
    if (isNaN(start) || isNaN(end)) return fallback;
    return writeSpan(start, end, Date.now(), undefined, prefs.hour12);
  }

  function clockOf(iso, fallback) {
    var at = Date.parse(iso);
    return isNaN(at) ? fallback : write('clock', at, Date.now(), undefined, prefs.hour12);
  }

  function use(next) {
    prefs = next;
  }

  return { use: use, write: write, writeSpan: writeSpan, apply: apply, watch: watch, span: span, clockOf: clockOf };
})();

function isoDate(d) {
  return d.getFullYear() + '-' + String(d.getMonth() + 1).padStart(2, '0') + '-' +
    String(d.getDate()).padStart(2, '0');
}

function previewCell(value) {
  return value === null ? '(none)' : value;
}

const CARD_KIND = Object.freeze({ EPIC: 'epic' });
const CARD_SUBMENU = Object.freeze({ MOVE: 'move', COLOR: 'color', EPIC: 'epic' });
const CARD_ACT = Object.freeze({ TIMER: 'timer', MOVE: 'move', COLOR: 'color', ADD_TO_EPIC: 'add-to-epic', ASSIGN_MEMBERS: 'assign-members', MAKE_EPIC: 'make-epic', GROUPING_DATE: 'grouping-date', COPY_HERE: 'copy-here', COPY: 'copy', TASK_URL: 'task-url', DELETE: 'delete' });
const BULK_ACT = Object.freeze({ MOVE: 'move', COLOR: 'color', MEMBERS: 'members', MEMBERS_MENU: 'members-menu', DELETE: 'delete' });
const BULK_SUB = Object.freeze({ COLOR: 'color', MEMBERS: 'members', UNASSIGN: 'unassign' });

function boardEventPlan(events, cardColumns) {
  const refresh = [];
  let reload = false;
  events.forEach((event) => {
    if (event.kind === 'task_added') {
      reload = true;
    } else if (event.kind === 'task_moved') {
      if (cardColumns[event.task_id] !== event.column_id) reload = true;
    } else if (event.task_id in cardColumns && !refresh.includes(event.task_id)) {
      refresh.push(event.task_id);
    }
  });
  return { refresh: refresh, reload: reload };
}

if (typeof module !== 'undefined') {
  module.exports = { LocalTime: LocalTime, isoDate: isoDate, previewCell: previewCell, CARD_KIND: CARD_KIND, CARD_SUBMENU: CARD_SUBMENU, CARD_ACT: CARD_ACT, BULK_ACT: BULK_ACT, BULK_SUB: BULK_SUB, boardEventPlan: boardEventPlan };
}
