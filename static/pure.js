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

  var ReportRange = (function () {
    var DAY_MS = 86400000;
    var MAX_YEARS = 3;
    var MAX_DAYS = 365 * MAX_YEARS;

    function roundTenth(n) { return Math.round(n * 10) / 10; }

    function dayEnds(start, end) {
      var ends = [];
      var d = new Date(start.getFullYear(), start.getMonth(), start.getDate(), 23, 59, 59, 999);
      var last = new Date(end.getFullYear(), end.getMonth(), end.getDate(), 23, 59, 59, 999);
      while (d.getTime() <= last.getTime()) {
        ends.push(d);
        d = new Date(d);
        d.setDate(d.getDate() + 1);
      }
      return ends;
    }

    return {
      DAY_MS: DAY_MS, MAX_YEARS: MAX_YEARS, MAX_DAYS: MAX_DAYS,
      roundTenth: roundTenth, dayEnds: dayEnds,
    };
  })();

  var ReportValue = (function () {
    return {
      count: {
        text: 'Task count',
        axis: 'Task count',
        unit: 'tasks',
        valueOf: function () { return 1; },
      },
      estimate: {
        text: 'Time estimate',
        axis: 'Time estimate (hours)',
        unit: 'hours',
        valueOf: function (t) {
          var hours = Number(t.estimate_hours);
          return hours > 0 ? hours : 0;
        },
      },
      points: {
        text: 'Points',
        axis: 'Points',
        unit: 'points',
        valueOf: function (t) {
          var points = Number(t.points);
          return points > 0 ? points : 0;
        },
      },
    };
  })();

  var ColumnHistory = (function () {
    function parseMoveDetail(detail) {
      // "Moved from {from} to {to}" — split on the last " to " so column
      // names containing " to " parse deterministically.
      var body = detail.slice('Moved from '.length);
      var i = body.lastIndexOf(' to ');
      if (i === -1) return null;
      return { from: body.slice(0, i), to: body.slice(i + 4) };
    }

    // The column name a history event recorded the task arriving in, or
    // null for other events.
    function recordedDestination(e) {
      if (e.kind === 'created' && e.detail.indexOf('Task created in ') === 0) {
        return e.detail.slice('Task created in '.length);
      }
      if (e.kind === 'moved' && e.detail.indexOf('Moved from ') === 0) {
        var m = parseMoveDetail(e.detail);
        return m ? m.to : null;
      }
      return null;
    }

    // bands are columns keyed by id. An event that carries a
    // column id joins that column's band even after the column is deleted,
    // and a deleted column's band takes the newest name recorded for its
    // id. Name-only history maps by current name, then former name, then a
    // deleted column's recorded names, else gets a band of its own.
    function columnLookup(data) {
      var label = {};
      var current = {};
      var byName = {};
      (data.columns || []).forEach(function (c) {
        label[c.id] = c.name;
        current[c.id] = true;
        byName[c.name] = c.id;
      });
      (data.columns || []).forEach(function (c) {
        (c.former_names || []).forEach(function (old) {
          if (byName[old] === undefined) byName[old] = c.id;
        });
      });
      var newest = {};
      (data.tasks || []).forEach(function (t) {
        (t.history || []).forEach(function (e) {
          var id = e.column_id;
          var name = recordedDestination(e);
          var at = Date.parse(e.created_at);
          if (!id || current[id] || name == null || isNaN(at)) return;
          if (!newest[id] || at >= newest[id].at) newest[id] = { at: at, name: name };
          if (byName[name] === undefined) byName[name] = id;
        });
      });
      Object.keys(newest).forEach(function (id) { label[id] = newest[id].name; });
      return { label: label, current: current, byName: byName };
    }

    // The band key of the column an event put the task in, or null.
    function eventKey(e, cols) {
      var name = recordedDestination(e);
      if (name == null) return null;
      if (e.column_id && cols.label[e.column_id] !== undefined) return e.column_id;
      if (cols.byName[name] !== undefined) return cols.byName[name];
      var key = 'name:' + name;
      cols.label[key] = name;
      return key;
    }

    // Per-task column timeline: [{at: ms, col: band key}] sorted ascending.
    // "created" events carry the initial column; "moved" events carry the
    // destination. Tasks with no usable events fall back to their current
    // column for the whole window.
    function timelineFor(task, cols) {
      var tl = [];
      (task.history || []).forEach(function (e) {
        var d = new Date(e.created_at);
        if (isNaN(d.getTime())) return;
        var key = eventKey(e, cols);
        if (key) tl.push({ at: d.getTime(), col: key });
      });
      tl.sort(function (a, b) { return a.at - b.at; });
      var home = cols.current[task.column_id] ? task.column_id : null;
      if (tl.length === 0) {
        var c = new Date(task.created_at);
        if (!isNaN(c.getTime()) && home) tl.push({ at: c.getTime(), col: home });
      }
      // the timeline ends in the task's current column, whatever
      // name its last event recorded.
      if (home && tl.length > 0) tl[tl.length - 1].col = home;
      return tl;
    }

    return { lookup: columnLookup, eventKey: eventKey, timelineFor: timelineFor, recordedDestination: recordedDestination };
  })();

if (typeof module !== 'undefined') {
  module.exports = { LocalTime: LocalTime, isoDate: isoDate, ReportRange: ReportRange, ReportValue: ReportValue, ColumnHistory: ColumnHistory, previewCell: previewCell, CARD_KIND: CARD_KIND, CARD_SUBMENU: CARD_SUBMENU, CARD_ACT: CARD_ACT, BULK_ACT: BULK_ACT, BULK_SUB: BULK_SUB, boardEventPlan: boardEventPlan };
}
