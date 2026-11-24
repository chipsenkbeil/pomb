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
    var at = iso instanceof Date ? iso.getTime() : Date.parse(iso);
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

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
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

  var ReportFile = (function () {
    var CRC_TABLE = (function () {
      var t = [];
      for (var n = 0; n < 256; n++) {
        var c = n;
        for (var k = 0; k < 8; k++) c = c & 1 ? 0xEDB88320 ^ (c >>> 1) : c >>> 1;
        t.push(c >>> 0);
      }
      return t;
    })();

    function crc32(bytes) {
      var c = 0xFFFFFFFF;
      for (var i = 0; i < bytes.length; i++) c = CRC_TABLE[(c ^ bytes[i]) & 0xFF] ^ (c >>> 8);
      return (c ^ 0xFFFFFFFF) >>> 0;
    }

    var LOCAL_HEADER_LEN = 30;
    var CENTRAL_ENTRY_LEN = 46;
    var END_RECORD_LEN = 22;
    var ZIP_VERSION = 20;
    var UTF8_FLAG = 0x0800;
    var DOS_DATE_1980 = 33;

    function zipStored(files) {
      var enc = new TextEncoder();
      var parts = [];
      var central = [];
      var offset = 0;
      files.forEach(function (f) {
        var name = enc.encode(f.name);
        var data = enc.encode(f.text);
        var crc = crc32(data);
        var head = new DataView(new ArrayBuffer(LOCAL_HEADER_LEN));
        head.setUint32(0, 0x04034b50, true);
        head.setUint16(4, ZIP_VERSION, true);
        head.setUint16(6, UTF8_FLAG, true);
        head.setUint16(12, DOS_DATE_1980, true);
        head.setUint32(14, crc, true);
        head.setUint32(18, data.length, true);
        head.setUint32(22, data.length, true);
        head.setUint16(26, name.length, true);
        var entry = new DataView(new ArrayBuffer(CENTRAL_ENTRY_LEN));
        entry.setUint32(0, 0x02014b50, true);
        entry.setUint16(4, ZIP_VERSION, true);
        entry.setUint16(6, ZIP_VERSION, true);
        entry.setUint16(8, UTF8_FLAG, true);
        entry.setUint16(14, DOS_DATE_1980, true);
        entry.setUint32(16, crc, true);
        entry.setUint32(20, data.length, true);
        entry.setUint32(24, data.length, true);
        entry.setUint16(28, name.length, true);
        entry.setUint32(42, offset, true);
        parts.push(new Uint8Array(head.buffer), name, data);
        central.push(new Uint8Array(entry.buffer), name);
        offset += LOCAL_HEADER_LEN + name.length + data.length;
      });
      var size = 0;
      central.forEach(function (p) { size += p.length; });
      var end = new DataView(new ArrayBuffer(END_RECORD_LEN));
      end.setUint32(0, 0x06054b50, true);
      end.setUint16(8, files.length, true);
      end.setUint16(10, files.length, true);
      end.setUint32(12, size, true);
      end.setUint32(16, offset, true);
      var all = parts.concat(central, [new Uint8Array(end.buffer)]);
      var total = 0;
      all.forEach(function (p) { total += p.length; });
      var out = new Uint8Array(total);
      var at = 0;
      all.forEach(function (p) { out.set(p, at); at += p.length; });
      return out;
    }

    var XLSX_CELL_MAX = 32767;

    function colLetters(i) {
      var s = '';
      for (var n = i + 1; n > 0; n = Math.floor((n - 1) / 26)) s = String.fromCharCode(65 + (n - 1) % 26) + s;
      return s;
    }

    function xmlText(s) {
      var text = String(s);
      if (text.length > XLSX_CELL_MAX) text = text.slice(0, XLSX_CELL_MAX - 4) + ' ...';
      return text.replace(/[\u0000-\u0008\u000B\u000C\u000E-\u001F\uFFFE\uFFFF]/g, '')
        .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
    }

    function xlsxBytes(rows, sheetName) {
      var sheet = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' +
        '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>';
      rows.forEach(function (r, y) {
        sheet += '<row r="' + (y + 1) + '">';
        r.forEach(function (v, x) {
          var ref = colLetters(x) + (y + 1);
          sheet += typeof v === 'number'
            ? '<c r="' + ref + '"><v>' + v + '</v></c>'
            : '<c r="' + ref + '" t="inlineStr"><is><t xml:space="preserve">' + xmlText(v == null ? '' : v) + '</t></is></c>';
        });
        sheet += '</row>';
      });
      sheet += '</sheetData></worksheet>';
      var head = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>';
      var rel = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships';
      return zipStored([
        { name: '[Content_Types].xml', text: head +
          '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">' +
          '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>' +
          '<Default Extension="xml" ContentType="application/xml"/>' +
          '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>' +
          '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>' +
          '</Types>' },
        { name: '_rels/.rels', text: head +
          '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' +
          '<Relationship Id="rId1" Type="' + rel + '/officeDocument" Target="xl/workbook.xml"/></Relationships>' },
        { name: 'xl/workbook.xml', text: head +
          '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="' + rel + '">' +
          '<sheets><sheet name="' + xmlText(sheetName) + '" sheetId="1" r:id="rId1"/></sheets></workbook>' },
        { name: 'xl/_rels/workbook.xml.rels', text: head +
          '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' +
          '<Relationship Id="rId1" Type="' + rel + '/worksheet" Target="worksheets/sheet1.xml"/></Relationships>' },
        { name: 'xl/worksheets/sheet1.xml', text: sheet },
      ]);
    }

    var XLSX_MIME = 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet';
    var PDF_PAGE_WIDTH = 612;
    var PDF_PAGE_HEIGHT = 792;
    var PDF_TITLE_PT = 16;
    var PDF_BODY_PT = 11;
    var PDF_LINES_PER_PAGE = 42;
    var PDF_LEFT = 50;
    var PDF_TITLE_Y = 750;
    var PDF_FIRST_LINE_Y = 715;
    var PDF_LEADING = 16;

    function pdfText(s) {
      return String(s).replace(/[^\x20-\x7E]/g, '?').replace(/[\\()]/g, '\\$&');
    }

    function pdfBytes(title, lines) {
      var pages = [];
      for (var at = 0; at < lines.length || !pages.length; at += PDF_LINES_PER_PAGE) {
        pages.push(lines.slice(at, at + PDF_LINES_PER_PAGE));
      }
      var objects = [
        '<< /Type /Catalog /Pages 2 0 R >>',
        '<< /Type /Pages /Kids [' + pages.map(function (_, i) { return (4 + 2 * i) + ' 0 R'; }).join(' ') +
          '] /Count ' + pages.length + ' >>',
        '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>',
      ];
      pages.forEach(function (page, i) {
        var stream = i === 0
          ? 'BT /F1 ' + PDF_TITLE_PT + ' Tf ' + PDF_LEFT + ' ' + PDF_TITLE_Y + ' Td (' + pdfText(title) + ') Tj ET\n'
          : '';
        page.forEach(function (line, k) {
          stream += 'BT /F1 ' + PDF_BODY_PT + ' Tf ' + PDF_LEFT + ' ' + (PDF_FIRST_LINE_Y - k * PDF_LEADING) +
            ' Td (' + pdfText(line) + ') Tj ET\n';
        });
        objects.push('<< /Type /Page /Parent 2 0 R /MediaBox [0 0 ' + PDF_PAGE_WIDTH + ' ' + PDF_PAGE_HEIGHT + '] /Contents ' + (5 + 2 * i) +
          ' 0 R /Resources << /Font << /F1 3 0 R >> >> >>');
        objects.push('<< /Length ' + stream.length + ' >>\nstream\n' + stream + 'endstream');
      });
      var out = '%PDF-1.4\n';
      var offsets = [];
      objects.forEach(function (body, i) {
        offsets.push(out.length);
        out += (i + 1) + ' 0 obj\n' + body + '\nendobj\n';
      });
      var xref = out.length;
      out += 'xref\n0 ' + (objects.length + 1) + '\n0000000000 65535 f \n';
      offsets.forEach(function (o) {
        out += ('0000000000' + o).slice(-10) + ' 00000 n \n';
      });
      out += 'trailer\n<< /Size ' + (objects.length + 1) + ' /Root 1 0 R >>\nstartxref\n' + xref + '\n%%EOF\n';
      var bytes = new Uint8Array(out.length);
      for (var c = 0; c < out.length; c++) bytes[c] = out.charCodeAt(c);
      return bytes;
    }

    return { xlsxBytes: xlsxBytes, pdfBytes: pdfBytes, crc32: crc32, XLSX_MIME: XLSX_MIME };
  })();

  var ReportChart = (function () {
    var SHORT_MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun',
      'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
    var LONG_MONTHS = ['January', 'February', 'March', 'April', 'May', 'June',
      'July', 'August', 'September', 'October', 'November', 'December'];

    function shortDate(d) {
      return SHORT_MONTHS[d.getMonth()] + ' ' + d.getDate();
    }

    function longDate(d) {
      return d.getDate() + ' ' + LONG_MONTHS[d.getMonth()] + ' ' + LocalTime.clockOf(d);
    }

    function dayStart(d) {
      return new Date(d.getFullYear(), d.getMonth(), d.getDate());
    }

    function pointX(geo, n, i) {
      var innerW = geo.W - geo.padL - geo.padR;
      return geo.padL + (n <= 1 ? innerW / 2 : (i / (n - 1)) * innerW);
    }

    function pointIndex(geo, n, rect, clientX, clientY) {
      if (n < 1 || rect.width <= 0 || rect.height <= 0) return -1;
      var px = (clientX - rect.left) * geo.W / rect.width;
      var py = (clientY - rect.top) * geo.H / rect.height;
      if (px < geo.padL || px > geo.W - geo.padR || py < geo.padT || py > geo.H - geo.padB) return -1;
      if (n === 1) return 0;
      var i = Math.round((px - geo.padL) / (geo.W - geo.padL - geo.padR) * (n - 1));
      return Math.max(0, Math.min(n - 1, i));
    }

    function crosshairLine(geo) {
      return '<line class="chart-crosshair" x1="0" x2="0" y1="' + geo.padT + '" y2="' +
        (geo.H - geo.padB) + '" visibility="hidden"/>';
    }

    function tipHtml(tip) {
      var html = '<div class="chart-tip-title">' + escapeHtml(tip.title) + '</div>';
      tip.rows.forEach(function (r) {
        html += '<div class="chart-tip-row">' +
          (r.color ? '<span class="chart-tip-swatch" style="background:' + escapeHtml(r.color) + '"></span>' : '') +
          '<span class="chart-tip-label">' + escapeHtml(r.label) + '</span>' +
          '<strong>' + escapeHtml(String(r.value)) + '</strong></div>';
      });
      return html;
    }

    function tipBox(host) {
      var box = host.querySelector('.chart-tip');
      if (!box) {
        box = document.createElement('div');
        box.className = 'chart-tip';
        box.hidden = true;
        host.appendChild(box);
      }
      return box;
    }

    function placeTip(host, box, clientX, clientY) {
      var rect = host.getBoundingClientRect();
      var left = clientX - rect.left + 14;
      if (left + box.offsetWidth > rect.width) left = clientX - rect.left - box.offsetWidth - 14;
      box.style.left = Math.max(0, left) + 'px';
      box.style.top = (clientY - rect.top + 14) + 'px';
    }

    function attachCrosshair(host, geo, n, describe) {
      var svg = host.querySelector('svg');
      var line = svg && svg.querySelector('.chart-crosshair');
      if (!line) return;
      var box = tipBox(host);
      function hide() {
        line.setAttribute('visibility', 'hidden');
        box.hidden = true;
      }
      svg.addEventListener('pointermove', function (e) {
        var i = pointIndex(geo, n, svg.getBoundingClientRect(), e.clientX, e.clientY);
        if (i < 0) { hide(); return; }
        var x = pointX(geo, n, i).toFixed(1);
        line.setAttribute('x1', x);
        line.setAttribute('x2', x);
        line.setAttribute('visibility', 'visible');
        box.innerHTML = tipHtml(describe(i));
        box.hidden = false;
        placeTip(host, box, e.clientX, e.clientY);
      });
      svg.addEventListener('pointerleave', hide);
    }

    function dotTip(el) {
      return {
        title: el.dataset.name,
        rows: [
          { label: 'From', value: el.dataset.from },
          { label: 'To', value: el.dataset.to },
          { label: 'Days', value: el.dataset.days },
        ],
      };
    }

    function attachDots(host, selector, onOpen) {
      function dotOf(e) {
        return e.target && e.target.closest ? e.target.closest(selector) : null;
      }
      function show(e) {
        var el = dotOf(e);
        if (!el) return;
        var box = tipBox(host);
        box.innerHTML = tipHtml(dotTip(el));
        box.hidden = false;
        placeTip(host, box, e.clientX, e.clientY);
      }
      host.addEventListener('pointerover', show);
      host.addEventListener('pointermove', show);
      host.addEventListener('pointerout', function (e) {
        if (dotOf(e)) tipBox(host).hidden = true;
      });
      host.addEventListener('click', function (e) {
        var el = dotOf(e);
        if (el) onOpen(el.dataset.taskId);
      });
      host.addEventListener('keydown', function (e) {
        var el = dotOf(e);
        if (el && (e.key === 'Enter' || e.key === ' ')) {
          e.preventDefault();
          onOpen(el.dataset.taskId);
        }
      });
    }

    return {
      shortDate: shortDate, longDate: longDate, dayStart: dayStart, pointX: pointX,
      pointIndex: pointIndex, crosshairLine: crosshairLine, tipHtml: tipHtml,
      attachCrosshair: attachCrosshair, dotTip: dotTip, attachDots: attachDots,
    };
  })();

if (typeof module !== 'undefined') {
  module.exports = { LocalTime: LocalTime, isoDate: isoDate, ReportRange: ReportRange, ReportValue: ReportValue, ColumnHistory: ColumnHistory, ReportFile: ReportFile, ReportChart: ReportChart, escapeHtml: escapeHtml, previewCell: previewCell, CARD_KIND: CARD_KIND, CARD_SUBMENU: CARD_SUBMENU, CARD_ACT: CARD_ACT, BULK_ACT: BULK_ACT, BULK_SUB: BULK_SUB, boardEventPlan: boardEventPlan };
}
