/* ChipFlow frontend glue: KanbanFlow-style board (drag-and-drop, column and
 * swimlane menus, task modal), plus the global timer.
 * No framework — plain JS plus SortableJS (drag-and-drop) and htmx
 * (add-task forms, rendered server-side).
 *
 * The timer is server-side: one active session, so it survives page
 * reloads. This file polls it, renders the header pill + popup, and
 * handles the "why did you stop?" flow. */
(function () {
  'use strict';

  var dragging = false;   // true while a Sortable drag is in flight (suppresses card clicks)
  var modalDirty = false; // set when the modal changed something; reloads the board on close

  // ---------- small helpers ----------

  function boardId() {
    var main = document.querySelector('.board-wrap');
    return main ? main.dataset.boardId : null;
  }

  function api(path, method, data) {
    return fetch(path, {
      method: method,
      headers: { 'Content-Type': 'application/json' },
      body: data === undefined ? undefined : JSON.stringify(data),
    });
  }

  function escapeHtml(s) {
    return String(s)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  var toastTimer = null;
  function toast(message) {
    var el = document.getElementById('toast');
    if (!el) return;
    el.innerHTML = message;
    el.hidden = false;
    if (toastTimer) window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(function () { el.hidden = true; }, 4000);
  }

  function cssEscape(s) {
    if (window.CSS && window.CSS.escape) return window.CSS.escape(s);
    return String(s).replace(/["\\]/g, '\\$&');
  }

  // ---------- task drag and drop ----------

  function initTaskSortable() {
    if (typeof Sortable === 'undefined') return;
    document.querySelectorAll('.task-list').forEach(function (list) {
      if (list._sortable) return;
      list._sortable = new Sortable(list, {
        group: 'tasks',
        animation: 150,
        draggable: '.task-card',
        ghostClass: 'sortable-placeholder',
        onStart: function () { dragging = true; },
        onEnd: function (evt) {
          window.setTimeout(function () { dragging = false; }, 80);
          var card = evt.item;
          var toList = evt.to;
          var fromList = evt.from;
          var cards = Array.prototype.slice.call(toList.querySelectorAll('.task-card'));
          var position = cards.indexOf(card);
          var fromCol = fromList.dataset.columnId;
          var toCol = toList.dataset.columnId;
          // Done-column date grouping can't be fixed by swapping one card;
          // reload the board when the move crosses the Done boundary.
          var crossesDone = (toList.closest('[data-done-column="true"]') != null) ||
                            (fromList.closest('[data-done-column="true"]') != null);
          fetch('/api/tasks/' + encodeURIComponent(card.dataset.taskId) + '/move', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              column_id: toCol,
              swimlane_id: toList.dataset.swimlaneId || null,
              position: position,
            }),
          }).then(function (res) {
            if (!res.ok) { window.location.reload(); return null; } // resync on failure
            return res.text();
          }).then(function (html) {
            if (html == null) return;
            if (crossesDone) { window.location.reload(); return; }
            // Swap in the refreshed card (Done stamp, totals) in place.
            var tmp = document.createElement('div');
            tmp.innerHTML = html;
            var fresh = tmp.firstElementChild;
            if (fresh) card.replaceWith(fresh);
            if (fromCol !== toCol) {
              updateColumnCount(fromCol, -1);
              updateColumnCount(toCol, 1);
            }
          }).catch(function () { window.location.reload(); });
        },
      });
    });
  }
  // Note: htmx inserts new cards inside the existing .task-list containers,
  // which already have Sortable attached, so no re-init is needed.

  function updateColumnCount(colId, delta) {
    var th = document.querySelector('.columnHeader[data-column-id="' + cssEscape(colId) + '"]');
    if (!th) return;
    var count = (parseInt(th.dataset.taskCount, 10) || 0) + delta;
    th.dataset.taskCount = String(count);
    var wip = th.dataset.wipLimit ? parseInt(th.dataset.wipLimit, 10) : null;
    var el = th.querySelector('.columnHeader-count');
    if (el) el.textContent = wip ? count + ' / ' + wip : String(count);
    var warn = wip != null && count >= wip;
    th.classList.toggle('columnHeader--warning', warn);
    th.classList.toggle('columnHeader--limitWarning', warn);
    var line = th.querySelector('.columnHeader-warningLine');
    if (warn && !line) {
      line = document.createElement('div');
      line.className = 'columnHeader-warningLine';
      line.setAttribute('aria-hidden', 'true');
      th.appendChild(line);
    } else if (!warn && line) {
      line.remove();
    }
  }

  // ---------- column drag reorder ----------

  function initColumnSortable() {
    if (typeof Sortable === 'undefined') return;
    var row = document.getElementById('column-headers-row');
    if (!row || row._sortable) return;
    row._sortable = new Sortable(row, {
      animation: 150,
      draggable: '.columnHeader',
      ghostClass: 'sortable-placeholder',
      // Let the header buttons work normally instead of starting a drag.
      filter: 'button',
      preventOnFilter: false,
      onEnd: function (evt) {
        var headers = Array.prototype.slice.call(row.querySelectorAll('.columnHeader'));
        var order = headers.map(function (h) { return h.dataset.columnId; });
        var newIndex = order.indexOf(evt.item.dataset.columnId);
        // Reorder every body row's cells to match the new header order.
        document.querySelectorAll('.board-table tbody tr').forEach(function (tr) {
          var tds = Array.prototype.slice.call(tr.querySelectorAll('td.board-cell'));
          order.forEach(function (id) {
            for (var i = 0; i < tds.length; i++) {
              if (tds[i].dataset.columnId === id) { tr.appendChild(tds[i]); break; }
            }
          });
        });
        api('/api/columns/' + encodeURIComponent(evt.item.dataset.columnId) + '/move',
            'POST', { position: newIndex })
          .then(function (res) { if (!res.ok) window.location.reload(); })
          .catch(function () { window.location.reload(); });
      },
    });
  }

  // Collapsed columns render a narrow header; hide their cells' task lists.
  function applyCollapsedColumns() {
    var headers = Array.prototype.slice.call(document.querySelectorAll('#column-headers-row .columnHeader'));
    document.querySelectorAll('.board-table tbody tr').forEach(function (tr) {
      var tds = tr.querySelectorAll('td.board-cell');
      headers.forEach(function (th, i) {
        if (th.dataset.collapsed === '1' && tds[i]) tds[i].classList.add('board-cell--collapsed');
      });
    });
  }

  // ---------- add-task form ----------

  function initAddTask() {
    document.addEventListener('click', function (e) {
      var btn = e.target.closest('[data-add-task-for]');
      if (!btn) return;
      // Only one open form per column.
      var colId = btn.getAttribute('data-add-task-for');
      var firstList = document.querySelector('.task-list[data-column-id="' + cssEscape(colId) + '"]');
      if (!firstList || firstList.querySelector('.add-task-form')) return;
      var tpl = document.getElementById('add-task-form-template');
      if (!tpl) return;
      var frag = tpl.content.cloneNode(true);
      var form = frag.querySelector('form');
      form.querySelector('input[name="column_id"]').value = colId;
      form.querySelector('input[name="swimlane_id"]').value = firstList.dataset.swimlaneId || '';
      firstList.insertBefore(frag, firstList.firstChild);
      if (window.htmx) window.htmx.process(firstList);
      var input = firstList.querySelector('.add-task-form input[name="name"]');
      if (input) input.focus();
    });

    document.addEventListener('click', function (e) {
      if (e.target.closest('[data-cancel-add]')) {
        var form = e.target.closest('.add-task-form');
        if (form) form.remove();
      }
    });

    // After htmx inserts the new card: drop the form, bump the count.
    document.addEventListener('htmx:afterRequest', function (e) {
      var form = e.target.closest ? e.target.closest('.add-task-form') : null;
      if (!form) return;
      var colId = form.querySelector('input[name="column_id"]').value;
      form.remove();
      if (e.detail && e.detail.successful) updateColumnCount(colId, 1);
    });
  }

  // ---------- floating menus ----------

  var openMenu = null;

  function hideFloatingMenus() {
    var any = false;
    document.querySelectorAll('.menu-pop.menu-floating, .details-popup').forEach(function (m) {
      if (!m.hidden) { m.hidden = true; any = true; }
    });
    if (openMenu) { openMenu = null; any = true; }
    return any;
  }

  function placeMenu(menu, x, y) {
    hideFloatingMenus();
    menu.hidden = false;
    menu.style.left = '0px';
    menu.style.top = '0px';
    var w = menu.offsetWidth;
    var h = menu.offsetHeight;
    menu.style.left = Math.max(4, Math.min(x, window.innerWidth - w - 4)) + 'px';
    menu.style.top = Math.max(4, Math.min(y, window.innerHeight - h - 4)) + 'px';
    openMenu = menu;
  }

  function closeAllTmMenus() {
    var any = false;
    document.querySelectorAll('.tm-menu').forEach(function (m) {
      if (!m.hidden) { m.hidden = true; any = true; }
    });
    return any;
  }

  var colMenuColumnId = null;
  var ctxMenuColumnId = null;
  var laneMenuSwimlaneId = null;

  function initBoardMenus() {
    // Column header ⋮ menu.
    document.addEventListener('click', function (e) {
      var btn = e.target.closest('[data-col-menu-for]');
      if (btn) {
        e.stopPropagation();
        var th = btn.closest('.columnHeader');
        colMenuColumnId = th ? th.dataset.columnId : null;
        var r = btn.getBoundingClientRect();
        placeMenu(document.getElementById('column-menu'), r.left, r.bottom + 4);
        return;
      }
      var laneBtn = e.target.closest('[data-lane-menu-for]');
      if (laneBtn) {
        e.stopPropagation();
        var label = laneBtn.closest('.swimlane-label');
        laneMenuSwimlaneId = label ? label.dataset.swimlaneId : null;
        var lr = laneBtn.getBoundingClientRect();
        placeMenu(document.getElementById('swimlane-menu'), lr.left, lr.bottom + 4);
        return;
      }
      // Click-away closes floating menus and task-modal submenus.
      if (openMenu && !e.target.closest('.menu-pop')) hideFloatingMenus();
      if (!e.target.closest('.tm-action')) closeAllTmMenus();
    });

    // Column header right-click menu (reference: no context menu on cards).
    document.addEventListener('contextmenu', function (e) {
      var th = e.target.closest('.columnHeader');
      if (!th) return;
      e.preventDefault();
      ctxMenuColumnId = th.dataset.columnId;
      var toggle = document.querySelector('#column-ctx-menu [data-ctx-act="collapse"]');
      if (toggle) toggle.textContent = th.dataset.collapsed === '1' ? 'Expand' : 'Collapse';
      placeMenu(document.getElementById('column-ctx-menu'), e.clientX, e.clientY);
    });

    document.getElementById('column-menu').addEventListener('click', function (e) {
      var btn = e.target.closest('[data-col-act]');
      if (!btn || !colMenuColumnId) return;
      var act = btn.getAttribute('data-col-act');
      var id = colMenuColumnId;
      hideFloatingMenus();
      if (act === 'edit') openEditColumnDialog(id);
      else if (act === 'left') moveColumnBy(id, -1);
      else if (act === 'right') moveColumnBy(id, 1);
      else if (act === 'add-left') openAddColumnDialog('beginning');
      else if (act === 'add-right') openAddColumnDialog('end');
      else if (act === 'delete') deleteColumn(id);
    });

    document.getElementById('column-ctx-menu').addEventListener('click', function (e) {
      var btn = e.target.closest('[data-ctx-act]');
      if (!btn || !ctxMenuColumnId) return;
      var act = btn.getAttribute('data-ctx-act');
      var id = ctxMenuColumnId;
      var th = document.querySelector('.columnHeader[data-column-id="' + cssEscape(id) + '"]');
      hideFloatingMenus();
      if (act === 'edit') openEditColumnDialog(id);
      else if (act === 'collapse' && th) {
        api('/api/columns/' + encodeURIComponent(id), 'PATCH',
            { collapsed: th.dataset.collapsed !== '1' })
          .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not update column.'); });
      } else if (act === 'details' && th) {
        document.getElementById('details-col-name').textContent = th.dataset.columnName || '';
        document.getElementById('details-col-count').textContent = th.dataset.taskCount || '0';
        var popup = document.getElementById('column-details-popup');
        var r = th.getBoundingClientRect();
        placeMenu(popup, r.left, r.bottom + 4);
      }
    });

    document.getElementById('swimlane-menu').addEventListener('click', function (e) {
      var btn = e.target.closest('[data-lane-act]');
      if (!btn || !laneMenuSwimlaneId) return;
      var act = btn.getAttribute('data-lane-act');
      var id = laneMenuSwimlaneId;
      hideFloatingMenus();
      if (act === 'rename') renameSwimlane(id);
      else if (act === 'up') moveSwimlane(id, -1);
      else if (act === 'down') moveSwimlane(id, 1);
      else if (act === 'delete') deleteSwimlane(id);
    });

    // Generic dialog wiring: [data-close-dialog] hides its overlay.
    document.addEventListener('click', function (e) {
      var closer = e.target.closest('[data-close-dialog]');
      if (closer) {
        var overlay = closer.closest('.dlg-overlay');
        if (overlay) overlay.hidden = true;
      }
    });

    document.getElementById('add-column-btn').addEventListener('click', function () {
      openAddColumnDialog('end');
    });
    document.getElementById('add-swimlane-btn').addEventListener('click', function () {
      addSwimlane();
    });
    document.getElementById('ac-add').addEventListener('click', doAddColumn);
    document.getElementById('ec-save').addEventListener('click', doSaveColumn);
    document.getElementById('mt-move-btn').addEventListener('click', doMoveTask);
    document.getElementById('est-add').addEventListener('click', doAddEstimate);
    document.getElementById('save-template-btn').addEventListener('click', function () {
      document.getElementById('st-name').value = '';
      document.getElementById('st-description').value = '';
      document.getElementById('save-template-dialog').hidden = false;
      document.getElementById('st-name').focus();
    });
    document.getElementById('st-save').addEventListener('click', doSaveTemplate);
  }

  // ---------- columns ----------

  function moveColumnBy(id, dir) {
    var headers = Array.prototype.slice.call(document.querySelectorAll('#column-headers-row .columnHeader'));
    var index = headers.findIndex(function (h) { return h.dataset.columnId === id; });
    if (index < 0) return;
    var target = index + dir;
    if (target < 0 || target >= headers.length) return;
    api('/api/columns/' + encodeURIComponent(id) + '/move', 'POST', { position: target })
      .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not move column.'); });
  }

  function deleteColumn(id) {
    var th = document.querySelector('.columnHeader[data-column-id="' + cssEscape(id) + '"]');
    var name = th ? th.dataset.columnName : id;
    var count = th ? parseInt(th.dataset.taskCount, 10) || 0 : 0;
    var msg = count > 0
      ? 'Delete column "' + name + '"? It still holds ' + count +
        ' task(s) — the server will refuse until they are moved or deleted.'
      : 'Delete column "' + name + '"?';
    if (!window.confirm(msg)) return;
    api('/api/columns/' + encodeURIComponent(id), 'DELETE')
      .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not delete column.'); });
  }

  function openAddColumnDialog(position) {
    document.getElementById('ac-name').value = '';
    document.getElementById('ac-position').value = position === 'beginning' ? 'beginning' : 'end';
    document.getElementById('add-column-dialog').hidden = false;
    document.getElementById('ac-name').focus();
  }

  function doAddColumn() {
    var name = document.getElementById('ac-name').value.trim();
    if (!name) { toast('Column name is required.'); return; }
    var atBeginning = document.getElementById('ac-position').value === 'beginning';
    api('/api/columns', 'POST', { board_id: boardId(), name: name })
      .then(function (res) { return res.ok ? res.json() : Promise.reject(new Error('create')); })
      .then(function (data) {
        if (atBeginning && data && data.id) {
          return api('/api/columns/' + encodeURIComponent(data.id) + '/move', 'POST', { position: 0 });
        }
        return null;
      })
      .then(function () { window.location.reload(); })
      .catch(function () { toast('Could not add column.'); });
  }

  var editColumnId = null;

  function openEditColumnDialog(id) {
    var th = document.querySelector('.columnHeader[data-column-id="' + cssEscape(id) + '"]');
    if (!th) return;
    editColumnId = id;
    document.getElementById('ec-name').value = th.dataset.columnName || '';
    document.getElementById('ec-description').value = th.dataset.columnDescription || '';
    document.getElementById('ec-wip').value = th.dataset.wipLimit || '';
    document.getElementById('ec-collapsed').checked = th.dataset.collapsed === '1';
    // The remaining fields live in the column's opaque config_json bag; no
    // single-column GET exists, so the dialog edits them from defaults.
    document.getElementById('ec-sorting').value = 'none';
    document.getElementById('ec-column-sum').checked = false;
    document.getElementById('ec-group-by-date').checked = false;
    document.getElementById('ec-show-description').checked = true;
    document.getElementById('ec-show-count').checked = true;
    document.getElementById('ec-show-wip').checked = true;
    document.getElementById('edit-column-dialog').hidden = false;
    document.getElementById('ec-name').focus();
  }

  function doSaveColumn() {
    if (!editColumnId) return;
    var name = document.getElementById('ec-name').value.trim();
    if (!name) { toast('Column name is required.'); return; }
    var wipRaw = document.getElementById('ec-wip').value.trim();
    var wip = wipRaw === '' ? null : parseInt(wipRaw, 10);
    if (wipRaw !== '' && (!wip || wip < 1)) { toast('WIP limit must be a positive number.'); return; }
    var config = {
      sorting: document.getElementById('ec-sorting').value,
      column_sum: document.getElementById('ec-column-sum').checked,
      group_by_date: document.getElementById('ec-group-by-date').checked,
      show_description: document.getElementById('ec-show-description').checked,
      show_task_count: document.getElementById('ec-show-count').checked,
      show_wip_limit: document.getElementById('ec-show-wip').checked,
    };
    api('/api/columns/' + encodeURIComponent(editColumnId), 'PATCH', {
      name: name,
      description: document.getElementById('ec-description').value.trim(),
      wip_limit: wip,
      collapsed: document.getElementById('ec-collapsed').checked,
      config_json: JSON.stringify(config),
    }).then(function (res) {
      if (res.ok) window.location.reload();
      else toast('Could not save column.');
    });
  }

  // ---------- swimlanes ----------

  function addSwimlane() {
    var name = window.prompt('New swimlane name:');
    if (!name || !name.trim()) return;
    api('/api/swimlanes', 'POST', { board_id: boardId(), name: name.trim() })
      .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not add swimlane.'); });
  }

  function renameSwimlane(id) {
    var label = document.querySelector('.swimlane-label[data-swimlane-id="' + cssEscape(id) + '"]');
    var current = label ? label.dataset.swimlaneName : '';
    var name = window.prompt('Rename swimlane:', current);
    if (name === null || !name.trim() || name.trim() === current) return;
    api('/api/swimlanes/' + encodeURIComponent(id), 'PATCH', { name: name.trim() })
      .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not rename swimlane.'); });
  }

  function moveSwimlane(id, dir) {
    var labels = Array.prototype.slice.call(document.querySelectorAll('.swimlane-label'));
    var index = labels.findIndex(function (l) { return l.dataset.swimlaneId === id; });
    if (index < 0) return;
    var target = index + dir;
    if (target < 0 || target >= labels.length) return;
    api('/api/swimlanes/' + encodeURIComponent(id) + '/move', 'POST', { position: target })
      .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not move swimlane.'); });
  }

  function deleteSwimlane(id) {
    var label = document.querySelector('.swimlane-label[data-swimlane-id="' + cssEscape(id) + '"]');
    var name = label ? label.dataset.swimlaneName : id;
    var count = label ? parseInt(label.dataset.taskCount, 10) || 0 : 0;
    var msg = count > 0
      ? 'Delete swimlane "' + name + '"? It still holds ' + count +
        ' task(s) — the server will refuse until they are moved or deleted.'
      : 'Delete swimlane "' + name + '"?';
    if (!window.confirm(msg)) return;
    api('/api/swimlanes/' + encodeURIComponent(id), 'DELETE')
      .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not delete swimlane.'); });
  }

  function moveTaskToDone(taskId) {
    var doneHeader = document.querySelector('.columnHeader[data-is-done="1"]');
    if (!doneHeader) return;
    var doneColId = doneHeader.dataset.columnId;
    var card = document.querySelector('.task-card[data-task-id="' + cssEscape(taskId) + '"]');
    var fromList = card ? card.closest('.task-list') : null;
    var swimlaneId = fromList ? (fromList.dataset.swimlaneId || null) : null;
    fetch('/api/tasks/' + encodeURIComponent(taskId) + '/move', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ column_id: doneColId, swimlane_id: swimlaneId, position: 0 }),
    }).then(function (res) {
      if (res.ok) window.location.reload();
    });
  }

  // ---------- save board as template ----------

  function doSaveTemplate() {
    var name = document.getElementById('st-name').value.trim();
    if (!name) { toast('Template name is required.'); return; }
    var description = document.getElementById('st-description').value.trim();
    api('/api/boards/' + encodeURIComponent(boardId()) + '/save-as-template',
        'POST', { name: name, description: description })
      .then(function (res) {
        if (!res.ok) throw new Error('save failed');
        document.getElementById('save-template-dialog').hidden = true;
        toast('Board saved as a template.');
      })
      .catch(function () { toast('Could not save template.'); });
  }

  // ---------- task modal ----------

  function modalTaskId() {
    var modal = document.querySelector('#modal-root .task-modal');
    return modal ? modal.dataset.taskId : null;
  }

  function openModal(taskId) {
    fetch('/api/tasks/' + encodeURIComponent(taskId) + '/modal', {
      headers: { 'Accept': 'text/html' },
    })
      .then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); })
      .then(function (html) {
        document.getElementById('modal-root').innerHTML = html;
        modalDirty = false;
        wireModal();
      })
      .catch(function () { /* leave the board as-is on failure */ });
  }

  function refreshModal() {
    var id = modalTaskId();
    if (id) openModal(id);
  }

  function closeModal() {
    var root = document.getElementById('modal-root');
    if (!root) return;
    root.innerHTML = '';
    if (modalDirty) window.location.reload();
  }

  function saveModalName() {
    var id = modalTaskId();
    var input = document.getElementById('modal-name');
    if (!id || !input) return;
    var name = input.value.trim();
    if (!name) { input.value = input.defaultValue; return; }
    api('/api/tasks/' + encodeURIComponent(id), 'PATCH', { name: name })
      .then(function (res) {
        if (res.ok) {
          modalDirty = true;
          input.defaultValue = name;
        }
      });
  }

  function saveModalDescription() {
    var id = modalTaskId();
    var input = document.getElementById('modal-description');
    if (!id || !input) return;
    api('/api/tasks/' + encodeURIComponent(id), 'PATCH', { description: input.value })
      .then(function (res) { if (res.ok) modalDirty = true; });
  }

  function deleteModalTask() {
    var id = modalTaskId();
    if (!id || !window.confirm('Delete this task and its time entries?')) return;
    api('/api/tasks/' + encodeURIComponent(id), 'DELETE')
      .then(function (res) {
        if (res.ok) { modalDirty = true; closeModal(); }
      });
  }

  function buildModalColorPicker() {
    var picker = document.getElementById('modal-color-picker');
    var src = document.getElementById('board-colors');
    if (!picker || !src) return;
    var modal = document.querySelector('#modal-root .task-modal');
    var current = modal ? modal.dataset.colorValue : null;
    picker.innerHTML = '';
    Array.prototype.forEach.call(src.querySelectorAll('span[data-id]'), function (s) {
      var b = document.createElement('button');
      b.type = 'button';
      b.className = 'tm-color-dot' + (s.dataset.value === current ? ' selected' : '');
      b.style.backgroundColor = s.dataset.bg;
      b.style.borderColor = s.dataset.border;
      b.title = s.dataset.label;
      b.setAttribute('aria-label', s.dataset.label);
      b.dataset.colorId = s.dataset.id;
      b.dataset.colorValue = s.dataset.value;
      b.addEventListener('click', function () { setModalColor(b); });
      picker.appendChild(b);
    });
  }

  function setModalColor(btn) {
    var id = modalTaskId();
    if (!id) return;
    api('/api/tasks/' + encodeURIComponent(id), 'PATCH', { color_id: btn.dataset.colorId })
      .then(function (res) {
        if (!res.ok) { toast('Could not change color.'); return; }
        var modal = document.querySelector('#modal-root .task-modal');
        var oldVal = modal ? modal.dataset.colorValue : null;
        if (modal && oldVal) {
          modal.classList.remove('taskColorVars-' + oldVal);
          modal.classList.add('taskColorVars-' + btn.dataset.colorValue);
          modal.dataset.colorValue = btn.dataset.colorValue;
        }
        Array.prototype.forEach.call(
          document.querySelectorAll('#modal-color-picker .tm-color-dot'),
          function (d) { d.classList.toggle('selected', d === btn); });
        var card = document.querySelector('.task-card[data-task-id="' + cssEscape(id) + '"]');
        if (card && oldVal) {
          card.classList.remove('taskColor-' + oldVal, 'taskBorderColor-' + oldVal);
          card.classList.add('taskColor-' + btn.dataset.colorValue,
                             'taskBorderColor-' + btn.dataset.colorValue);
          card.dataset.colorValue = btn.dataset.colorValue;
          card.title = btn.title;
        }
        modalDirty = true;
      });
  }

  function modalAction(act) {
    var id = modalTaskId();
    closeAllTmMenus();
    if (act === 'manual-time') {
      var nameInput = document.getElementById('modal-name');
      ManualTime.open(id, nameInput ? nameInput.value : '');
    } else if (act === 'estimate') {
      document.getElementById('est-input').value = '';
      document.getElementById('estimate-dialog').hidden = false;
      document.getElementById('est-input').focus();
    } else if (act === 'move') {
      openMoveDialog();
    } else if (act === 'start-pomodoro') {
      TimerUI.start('pomodoro', id);
    } else if (act === 'start-stopwatch') {
      TimerUI.start('stopwatch', id);
    } else if (act === 'time-log') {
      var heading = document.getElementById('time-log-heading');
      if (heading) heading.scrollIntoView({ behavior: 'smooth', block: 'start' });
    } else if (act === 'print') {
      window.print();
    } else if (act === 'watch') {
      toast('Task watching is not supported yet.');
    } else if (act === 'task-url') {
      copyTaskUrl(id);
    } else if (act === 'copy') {
      copyModalTask();
    } else if (act === 'shortcuts') {
      document.getElementById('shortcuts-dialog').hidden = false;
    } else if (act === 'delete') {
      deleteModalTask();
    }
  }

  function copyTaskUrl(id) {
    if (!id) return;
    var url = window.location.origin + '/b/' + boardId() + '#task-' + id;
    function done() { toast('Task URL copied to clipboard.'); }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(url).then(done, function () { toast(url); });
    } else {
      toast(url);
    }
  }

  function copyModalTask() {
    var id = modalTaskId();
    if (!id) return;
    var card = document.querySelector('.task-card[data-task-id="' + cssEscape(id) + '"]');
    var list = card ? card.closest('.task-list') : null;
    if (!list) { toast('Could not copy task.'); return; }
    var nameInput = document.getElementById('modal-name');
    var name = nameInput ? nameInput.value.trim() : 'Task';
    var modal = document.querySelector('#modal-root .task-modal');
    var colorId = null;
    if (modal) {
      var src = document.querySelector('#board-colors span[data-value="' + cssEscape(modal.dataset.colorValue) + '"]');
      if (src) colorId = src.dataset.id;
    }
    api('/api/tasks', 'POST', {
      column_id: list.dataset.columnId,
      swimlane_id: list.dataset.swimlaneId || null,
      name: name + ' (copy)',
      color_id: colorId,
    }).then(function (res) {
      if (res.ok) { modalDirty = true; toast('Task copied.'); }
      else toast('Could not copy task.');
    });
  }

  function openMoveDialog() {
    var id = modalTaskId();
    if (!id) return;
    var colSel = document.getElementById('mt-col');
    colSel.innerHTML = '';
    document.querySelectorAll('#column-headers-row .columnHeader').forEach(function (th) {
      var o = document.createElement('option');
      o.value = th.dataset.columnId;
      o.textContent = th.dataset.columnName;
      colSel.appendChild(o);
    });
    var laneSel = document.getElementById('mt-lane');
    laneSel.innerHTML = '';
    document.querySelectorAll('#board-swimlanes span[data-id]').forEach(function (s) {
      var o = document.createElement('option');
      o.value = s.dataset.id;
      o.textContent = s.dataset.name;
      laneSel.appendChild(o);
    });
    var card = document.querySelector('.task-card[data-task-id="' + cssEscape(id) + '"]');
    var list = card ? card.closest('.task-list') : null;
    if (list) {
      colSel.value = list.dataset.columnId;
      if (list.dataset.swimlaneId) laneSel.value = list.dataset.swimlaneId;
    }
    document.getElementById('mt-pos').value = 'bottom';
    document.getElementById('move-task-dialog').hidden = false;
  }

  function doMoveTask() {
    var id = modalTaskId();
    if (!id) return;
    var colId = document.getElementById('mt-col').value;
    var laneId = document.getElementById('mt-lane').value || null;
    var position = 0;
    if (document.getElementById('mt-pos').value === 'bottom') {
      var sel = '.task-list[data-column-id="' + cssEscape(colId) + '"]';
      if (laneId) sel += '[data-swimlane-id="' + cssEscape(laneId) + '"]';
      var list = document.querySelector(sel);
      position = list ? list.querySelectorAll('.task-card').length : 0;
    }
    api('/api/tasks/' + encodeURIComponent(id) + '/move', 'POST',
        { column_id: colId, swimlane_id: laneId, position: position })
      .then(function (res) {
        document.getElementById('move-task-dialog').hidden = true;
        if (res.ok) { modalDirty = true; closeModal(); }
        else toast('Could not move task.');
      });
  }

  // "Add time estimate": no dedicated backend field exists, so the estimate
  // maps onto the task's pomodoro size (the modal's Estimate row).
  function doAddEstimate() {
    var id = modalTaskId();
    if (!id) return;
    var raw = document.getElementById('est-input').value;
    var minutes = parseEstimateMinutes(raw);
    if (!minutes || minutes <= 0) { toast('Enter an estimate like 2h, 30m, or 1h 30m.'); return; }
    var pomodoroMinutes = (TimerUI.settings && TimerUI.settings.pomodoro_minutes) || 25;
    var pomodori = Math.max(1, Math.round(minutes / pomodoroMinutes));
    api('/api/tasks/' + encodeURIComponent(id), 'PATCH', { size: pomodori })
      .then(function (res) {
        document.getElementById('estimate-dialog').hidden = true;
        if (res.ok) { modalDirty = true; refreshModal(); }
        else toast('Could not save estimate.');
      });
  }

  function parseEstimateMinutes(raw) {
    var total = 0;
    var h = /(\d+(?:\.\d+)?)\s*h/i.exec(raw || '');
    var m = /(\d+)\s*m/i.exec(raw || '');
    if (h) total += parseFloat(h[1]) * 60;
    if (m) total += parseInt(m[1], 10);
    if (!h && !m && /^\d+$/.test((raw || '').trim())) total = parseInt(raw.trim(), 10);
    return Math.round(total);
  }

  function wireModal() {
    var overlay = document.getElementById('modal-overlay');
    if (!overlay) return;
    overlay.addEventListener('click', function (e) {
      if (e.target === overlay) closeModal();
    });
    buildModalColorPicker();
    var nameInput = document.getElementById('modal-name');
    if (nameInput) nameInput.addEventListener('change', saveModalName);
    var descInput = document.getElementById('modal-description');
    if (descInput) descInput.addEventListener('change', saveModalDescription);
    var closeBtn = overlay.querySelector('[data-close-modal]');
    if (closeBtn) closeBtn.addEventListener('click', closeModal);
    overlay.querySelectorAll('[data-tm-menu]').forEach(function (btn) {
      btn.addEventListener('click', function (e) {
        e.stopPropagation();
        var menu = document.getElementById(btn.getAttribute('data-tm-menu'));
        if (!menu) return;
        var wasHidden = menu.hidden;
        closeAllTmMenus();
        menu.hidden = !wasHidden;
      });
    });
    overlay.querySelectorAll('[data-tm-act]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        modalAction(btn.getAttribute('data-tm-act'));
      });
    });
  }

  // ---------- Timer UI (header pill + popup) ----------

  var TimerUI = {
    settings: null,
    state: null,
    pollHandle: null,
    tickHandle: null,
    lastStatus: null,
    // why-stop flow
    whyOriginal: null,
    whySessionId: null,
    whyEntryId: null,
    whyMode: null,
    whyTaskId: null,
    whySeconds: 0,
    whyStartWall: null,
    whyTaskName: 'Pomodoro',
    whyTickHandle: null,

    init: function () {
      var self = this;
      fetch('/api/timer/settings', { headers: { 'Accept': 'application/json' } })
        .then(function (res) { return res.ok ? res.json() : null; })
        .then(function (settings) {
          if (settings) self.settings = settings;
          self.refresh();
          self.pollHandle = window.setInterval(function () { self.refresh(); }, 15000);
        });
      var pill = document.getElementById('timer-pill');
      if (pill) {
        pill.addEventListener('click', function (e) {
          e.stopPropagation();
          self.togglePopup();
        });
      }
      document.addEventListener('click', function (e) {
        var popup = document.getElementById('timer-popup');
        if (popup && !popup.hidden &&
            !e.target.closest('#timer-popup') && !e.target.closest('#timer-pill')) {
          self.closePopup();
        }
      });
      // Esc for popup/why menu is handled by the unified Escape handler.
    },

    refresh: function () {
      var self = this;
      fetch('/api/timer/status', { headers: { 'Accept': 'application/json' } })
        .then(function (res) { return res.ok ? res.json() : null; })
        .then(function (status) {
          if (status) self.updateFromStatus(status);
        });
    },

    updateFromStatus: function (status) {
      this.lastStatus = status;
      var was = this.state ? this.state.phase : null;
      this.state = {
        phase: status.phase,
        taskId: status.task_id,
        taskName: status.task_name,
        remainingSeconds: status.remaining_seconds,
        totalSeconds: status.total_seconds,
        mode: status.mode,
        taskUrl: status.task_url,
        sessionId: status.session_id,
        pomodoroCount: status.pomodoro_count,
      };
      if (was && was !== 'idle' && status.phase === 'idle') {
        // Timer finished remotely; let the popup settle on its next open.
      }
      this.renderPill();
      if (document.getElementById('timer-popup') &&
          !document.getElementById('timer-popup').hidden) {
        this.renderPopup();
      }
      this.updateCardIndicators();
    },

    updateCardIndicators: function () {
      var self = this;
      var activeId = self.state && self.state.taskId ? String(self.state.taskId) : null;
      document.querySelectorAll('.task-card .card-timer-indicator').forEach(function (el) {
        var card = el.closest('.task-card');
        var show = activeId && card && String(card.dataset.taskId) === activeId;
        el.hidden = !show;
      });
    },

    fmt: function (seconds) {
      seconds = Math.max(0, Math.floor(seconds));
      var h = Math.floor(seconds / 3600);
      var m = Math.floor((seconds % 3600) / 60);
      var s = seconds % 60;
      var mm = (h > 0 && m < 10 ? '0' : '') + m;
      var ss = (s < 10 ? '0' : '') + s;
      return (h > 0 ? h + ':' + (m < 10 ? '0' + m : m) : mm) + ':' + ss;
    },

    pillLabel: function () {
      if (!this.state || this.state.phase === 'idle') return 'Pomodoro';
      var s = this.state;
      if (s.phase === 'running') {
        return 'Stop (' + this.fmt(s.remainingSeconds) + ')';
      }
      if (s.phase === 'paused') {
        return 'Resume (' + this.fmt(s.remainingSeconds) + ')';
      }
      return 'Pomodoro';
    },

    renderPill: function () {
      var pill = document.getElementById('timer-pill');
      if (!pill) return;
      pill.textContent = this.pillLabel();
      pill.classList.toggle('running', !!(this.state && this.state.phase !== 'idle'));
      var st = document.getElementById('timer-status');
      if (st) {
        if (this.state && this.state.phase !== 'idle') {
          var label = this.state.mode === 'pomodoro' ? 'Focus' :
                      this.state.mode === 'stopwatch' ? 'Stopwatch' : 'Break';
          st.textContent = label + ' — ' +
            (this.state.taskName || 'Pomodoro') + ' ' + this.fmt(this.state.remainingSeconds);
        } else {
          st.textContent = '';
        }
      }
    },

    renderPopup: function () {
      var popup = document.getElementById('timer-popup');
      if (!popup) return;
      var s = this.state;
      var body = document.getElementById('timer-popup-body');
      var modes = popup.querySelector('.timer-modes');
      var settingsLink = document.getElementById('timer-settings-link');
      if (!s || s.phase === 'idle') {
        if (body) body.innerHTML = '';
        if (modes) modes.hidden = false;
        this.renderModeTab();
        if (settingsLink) settingsLink.hidden = false;
        return;
      }
      if (settingsLink) settingsLink.hidden = true;
      if (modes) modes.hidden = true;
      var taskName = s.taskName || 'Pomodoro';
      var modeLabel = s.mode === 'pomodoro' ? 'Pomodoro' :
                      s.mode === 'stopwatch' ? 'Stopwatch' :
                      s.mode === 'short_break' ? 'Short break' :
                      s.mode === 'long_break' ? 'Long break' : s.mode;
      var pomodoros = this.pomodoroDots(s.pomodoroCount || 0);
      body.innerHTML =
        '<div class="timer-session">' +
          '<div class="timer-session-mode">' + escapeHtml(modeLabel) + '</div>' +
          '<div class="timer-session-time">' + this.fmt(s.remainingSeconds) + '</div>' +
          '<div class="timer-session-task">' + escapeHtml(taskName) + '</div>' +
          '<div class="timer-session-poms">' + pomodoros + '</div>' +
          '<div class="timer-session-actions">' +
            (s.phase === 'running'
              ? '<button type="button" class="btn btn-primary" id="tp-pause">Pause</button>' +
                '<button type="button" class="btn" id="tp-stop">Stop</button>'
              : '<button type="button" class="btn btn-primary" id="tp-resume">Resume</button>' +
                '<button type="button" class="btn" id="tp-stop">Stop</button>') +
            '<button type="button" class="btn btn-link" id="tp-switch">Switch task</button>' +
          '</div>' +
        '</div>';
      var pauseBtn = document.getElementById('tp-pause');
      if (pauseBtn) pauseBtn.addEventListener('click', this.pause.bind(this));
      var resumeBtn = document.getElementById('tp-resume');
      if (resumeBtn) resumeBtn.addEventListener('click', this.resume.bind(this));
      var stopBtn = document.getElementById('tp-stop');
      if (stopBtn) stopBtn.addEventListener('click', this.stopClicked.bind(this));
      var switchBtn = document.getElementById('tp-switch');
      if (switchBtn) switchBtn.addEventListener('click', this.changeTask.bind(this));
      this.startTick();
    },

    pomodoroDots: function (count) {
      var out = '';
      for (var i = 0; i < 4; i++) {
        out += '<span class="pom-dot' + (i < (count % 4 || (count > 0 ? 4 : 0)) ? ' filled' : '') + '"></span>';
      }
      return out;
    },

    startTick: function () {
      var self = this;
      if (self.tickHandle) window.clearInterval(self.tickHandle);
      self.tickHandle = window.setInterval(function () { self.tick(); }, 1000);
    },

    tick: function () {
      if (!this.state || this.state.phase !== 'running') return;
      this.state.remainingSeconds -= 1;
      if (this.state.remainingSeconds <= 0) {
        this.finishSession();
        return;
      }
      this.renderPill();
      var timeEl = document.querySelector('#timer-popup .timer-session-time');
      if (timeEl) timeEl.textContent = this.fmt(this.state.remainingSeconds);
    },

    togglePopup: function () {
      var popup = document.getElementById('timer-popup');
      if (!popup) return;
      if (popup.hidden) {
        popup.hidden = false;
        this.renderPopup();
        this.positionPopup();
      } else {
        this.closePopup();
      }
    },

    positionPopup: function () {
      var popup = document.getElementById('timer-popup');
      var pill = document.getElementById('timer-pill');
      if (!popup || !pill) return;
      var r = pill.getBoundingClientRect();
      popup.style.left = Math.max(8, Math.min(r.left, window.innerWidth - 260)) + 'px';
      popup.style.top = (r.bottom + 8) + 'px';
    },

    closePopup: function () {
      var popup = document.getElementById('timer-popup');
      if (popup) popup.hidden = true;
      if (this.tickHandle) { window.clearInterval(this.tickHandle); this.tickHandle = null; }
    },

    playChime: function () {
      try {
        var Ctx = window.AudioContext || window.webkitAudioContext;
        if (!Ctx) return;
        var ctx = new Ctx();
        var o = ctx.createOscillator();
        var g = ctx.createGain();
        o.connect(g); g.connect(ctx.destination);
        o.frequency.value = 880;
        g.gain.setValueAtTime(0.001, ctx.currentTime);
        g.gain.exponentialRampToValueAtTime(0.4, ctx.currentTime + 0.05);
        g.gain.exponentialRampToValueAtTime(0.001, ctx.currentTime + 1.2);
        o.start(); o.stop(ctx.currentTime + 1.3);
      } catch (e) { /* audio is best-effort */ }
    },

    currentModeTab: 'pomodoro',

    renderModeTab: function () {
      var tab = document.getElementById('timer-mode-tab');
      if (!tab || !this.settings) return;
      if (this.currentModeTab === 'pomodoro') {
        tab.innerHTML =
          '<label class="timer-label">Task' +
            '<select id="tt-task" class="timer-select"></select></label>' +
          '<label class="timer-label">Duration' +
            '<select id="tt-duration" class="timer-select">' +
              '<option value="15">15 min</option>' +
              '<option value="25"' + (this.settings.pomodoro_minutes === 25 ? ' selected' : '') + '>25 min</option>' +
              '<option value="50">50 min</option>' +
            '</select></label>' +
          '<div class="timer-actions">' +
            '<button type="button" class="btn btn-primary" id="tt-start">Start Pomodoro</button>' +
            '<button type="button" class="btn" id="tt-log">Time log</button>' +
          '</div>';
        var dur = document.getElementById('tt-duration');
        if (dur && this.settings.pomodoro_minutes !== 25 && this.settings.pomodoro_minutes !== 15 && this.settings.pomodoro_minutes !== 50) {
          var opt = document.createElement('option');
          opt.value = String(this.settings.pomodoro_minutes);
          opt.textContent = this.settings.pomodoro_minutes + ' min';
          opt.selected = true;
          dur.appendChild(opt);
        }
      } else {
        tab.innerHTML =
          '<label class="timer-label">Task' +
            '<select id="tt-task" class="timer-select"></select></label>' +
          '<div class="timer-actions">' +
            '<button type="button" class="btn btn-primary" id="tt-start">Start Stopwatch</button>' +
            '<button type="button" class="btn" id="tt-log">Time log</button>' +
          '</div>';
      }
      this.fillTaskOptions(document.getElementById('tt-task'));
      var start = document.getElementById('tt-start');
      if (start) {
        start.addEventListener('click', this.startIdle.bind(this));
      }
      var log = document.getElementById('tt-log');
      if (log) log.addEventListener('click', this.openLog.bind(this));
    },

    fillTaskOptions: function (select) {
      if (!select) return;
      var modalId = modalTaskId();
      var opts = '<option value="">(no task)</option>';
      document.querySelectorAll('.task-card').forEach(function (card) {
        var id = card.dataset.taskId;
        var nameEl = card.querySelector('.card-title');
        var name = nameEl ? nameEl.textContent.trim() : id;
        opts += '<option value="' + id + '"' + (id === modalId ? ' selected' : '') + '>' +
          escapeHtml(name) + '</option>';
      });
      select.innerHTML = opts;
    },

    switchModeTab: function () {
      this.currentModeTab = this.currentModeTab === 'pomodoro' ? 'stopwatch' : 'pomodoro';
      document.querySelectorAll('#timer-popup .timer-modes button').forEach(function (b, i, arr) {
        b.classList.toggle('active', (i === 0) === (TimerUI.currentModeTab === 'pomodoro'));
      });
      this.renderModeTab();
    },

    selectedTask: function () {
      var sel = document.getElementById('tt-task');
      return sel && sel.value ? sel.value : null;
    },

    startIdle: function () {
      var mode = this.currentModeTab;
      var taskId = this.selectedTask();
      var minutes = null;
      if (mode === 'pomodoro') {
        var dur = document.getElementById('tt-duration');
        minutes = dur ? parseInt(dur.value, 10) : (this.settings ? this.settings.pomodoro_minutes : 25);
      }
      this.start(mode, taskId, minutes);
    },

    start: function (mode, taskId, minutes) {
      var self = this;
      fetch('/api/timer/start', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          mode: mode,
          task_id: taskId || null,
          duration_minutes: minutes === undefined ? null : minutes,
        }),
      }).then(function (res) {
        if (res.ok) {
          self.refresh();
          self.closePopup();
        } else {
          res.text().then(function (t) { toast('Could not start timer: ' + t); });
        }
      });
    },

    startForTask: function (taskId, taskName) {
      // Quick-start a Pomodoro from a card; hide the now-obsolete menu.
      var menu = document.getElementById('tm-timer-menu');
      if (menu) menu.hidden = true;
      this.start('pomodoro', taskId);
    },

    startBreak: function (kind) {
      this.start(kind === 'long' ? 'long_break' : 'short_break', null);
    },

    pause: function () {
      var self = this;
      fetch('/api/timer/pause', { method: 'POST' })
        .then(function () { self.refresh(); });
    },

    resume: function () {
      var self = this;
      fetch('/api/timer/resume', { method: 'POST' })
        .then(function () { self.refresh(); });
    },

    stopClicked: function () {
      var s = this.state;
      if (!s || s.phase === 'idle') return;
      this.beginWhy(s.sessionId, s.taskId, s.taskName, s.mode, s.remainingSeconds, s.totalSeconds);
    },

    finishSession: function () {
      var self = this;
      if (self.tickHandle) { window.clearInterval(self.tickHandle); self.tickHandle = null; }
      fetch('/api/timer/stop', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ reason: 'completed' }),
      }).then(function () {
        self.playChime();
        self.refresh();
        self.closePopup();
      });
    },

    // ----- "why did you stop?" flow -----

    beginWhy: function (sessionId, taskId, taskName, mode, remainingSeconds, totalSeconds) {
      var self = this;
      this.whyOriginal = this.state;
      this.whySessionId = sessionId;
      this.whyTaskId = taskId;
      this.whyTaskName = taskName || 'Pomodoro';
      this.whyMode = mode;
      this.whyStartWall = Date.now();
      // seconds already elapsed (so the menu keeps counting while open)
      this.whySeconds = Math.max(0, (totalSeconds || 0) - (remainingSeconds || 0));
      this.whyEntryId = null;
      var menu = document.getElementById('why-stop-menu');
      var elapsed = document.getElementById('why-elapsed');
      menu.hidden = false;
      document.getElementById('why-task-name').textContent = this.whyTaskName;
      if (elapsed) elapsed.textContent = this.fmt(this.whySeconds);
      if (this.whyTickHandle) window.clearInterval(this.whyTickHandle);
      this.whyTickHandle = window.setInterval(function () {
        self.whySeconds += 1;
        if (elapsed) elapsed.textContent = self.fmt(self.whySeconds);
      }, 1000);
      // Close the timer popup underneath; the why menu takes over.
      this.closePopup();
    },

    closeWhyMenu: function () {
      var menu = document.getElementById('why-stop-menu');
      if (menu) menu.hidden = true;
      if (this.whyTickHandle) { window.clearInterval(this.whyTickHandle); this.whyTickHandle = null; }
    },

    stopAndLog: function (reason) {
      var self = this;
      fetch('/api/timer/stop', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ reason: reason }),
      }).then(function (res) { return res.ok ? res.json() : null; })
        .then(function (data) {
          if (data && data.entry_id) self.whyEntryId = data.entry_id;
          self.closeWhyMenu();
          self.refresh();
        });
    },

    addWhyReason: function (reason) {
      this.stopAndLog(reason);
    },

    whyTaskDone: function () {
      var self = this;
      var taskId = this.whyTaskId;
      this.stopAndLog('completed');
      if (taskId) {
        // Mark the task complete after the entry is logged.
        window.setTimeout(function () { moveTaskToDone(taskId); }, 400);
      }
    },

    changeTask: function () {
      var self = this;
      var s = this.state;
      if (!s) return;
      var lines = [];
      document.querySelectorAll('.task-card').forEach(function (card, i) {
        var nameEl = card.querySelector('.card-title');
        lines.push((i + 1) + '. ' + (nameEl ? nameEl.textContent.trim() : card.dataset.taskId));
      });
      if (!lines.length) { toast('No tasks on this board.'); return; }
      var raw = window.prompt('Move the timer to which task?\n' + lines.join('\n'));
      if (!raw) return;
      var idx = parseInt(raw, 10) - 1;
      var cards = document.querySelectorAll('.task-card');
      if (idx < 0 || idx >= cards.length) return;
      var newId = cards[idx].dataset.taskId;
      fetch('/api/timer/change-task', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ task_id: newId }),
      }).then(function (res) {
        if (!res.ok && res.status === 409) {
          if (!window.confirm('Are you sure you want to switch tasks mid-Pomodoro?')) return;
          return fetch('/api/timer/change-task', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ task_id: newId, force: true }),
          });
        }
        return res;
      }).then(function () { self.refresh(); });
    },

    addTime: function () {
      var id = modalTaskId();
      var nameInput = document.getElementById('modal-name');
      ManualTime.open(id, nameInput ? nameInput.value : null);
    },

    openLog: function () {
      window.location.href = '/timer/log';
    },
  };

  // ---------- small shared helpers ----------

  var taskNameCache = {};

  function fetchHtmlInto(url, targetSel) {
    fetch(url, { headers: { 'Accept': 'text/html' } })
      .then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); })
      .then(function (html) {
        var el = document.querySelector(targetSel);
        if (el) el.innerHTML = html;
      });
  }

  function refreshModalTimeLog() {
    var id = modalTaskId();
    if (id) fetchHtmlInto('/api/tasks/' + encodeURIComponent(id) + '/time-entries', '#modal-time-log');
  }

  function positionCalendar(input, cal) {
    var r = input.getBoundingClientRect();
    cal.style.left = Math.min(r.left, window.innerWidth - 260) + 'px';
    cal.style.top = (r.bottom + 6 + window.scrollY) + 'px';
  }

  // ---------- Manual time dialog ----------

  var ManualTime = {
    taskId: null,
    open: function (taskId, taskName) {
      this.taskId = taskId || null;
      document.getElementById('manual-time-overlay').hidden = false;
      var taskEl = document.getElementById('manual-time-task');
      var label = taskId ? (taskName || taskNameCache[taskId] || 'task') : '(no task)';
      taskEl.textContent = label;
      var today = new Date();
      var iso = today.getFullYear() + '-' +
        String(today.getMonth() + 1).padStart(2, '0') + '-' +
        String(today.getDate()).padStart(2, '0');
      document.getElementById('manual-time-date').value = iso;
      document.getElementById('manual-time-hours').value = '';
      document.getElementById('manual-time-minutes').value = '';
      document.getElementById('manual-time-comment').value = '';
      document.getElementById('manual-time-error').hidden = true;
      this.updateDuration();
    },
    close: function () {
      document.getElementById('manual-time-overlay').hidden = true;
    },
    closeError: function () {
      document.getElementById('manual-time-error').hidden = true;
    },
    showError: function (message) {
      // Server said the date is in the future (clock drift?) — show the
      // dedicated error dialog instead of the inline hint.
      if (message && /future/i.test(message)) {
        document.getElementById('manual-time-overlay').hidden = true;
        document.getElementById('future-time-overlay').hidden = false;
        return;
      }
      var err = document.getElementById('manual-time-error');
      err.textContent = message;
      err.hidden = false;
    },
    updateDuration: function () {
      var h = parseInt(document.getElementById('manual-time-hours').value, 10) || 0;
      var m = parseInt(document.getElementById('manual-time-minutes').value, 10) || 0;
      var total = h * 60 + m;
      var label = total > 0 ? total + ' minutes' : '—';
      document.getElementById('manual-time-duration').textContent = label;
    },
    openCalendar: function () {
      var input = document.getElementById('manual-time-date');
      var cal = document.getElementById('manual-time-calendar');
      // Toggle off if already open.
      if (!cal.hidden) { cal.hidden = true; return; }
      var current = input.value ? new Date(input.value + 'T12:00:00') : new Date();
      this.renderCalendar(current.getFullYear(), current.getMonth());
      positionCalendar(input, cal);
      cal.hidden = false;
    },
    renderCalendar: function (year, month) {
      var cal = document.getElementById('manual-time-calendar');
      var input = document.getElementById('manual-time-date');
      var first = new Date(year, month, 1);
      // Monday-first week grid.
      var startOffset = (first.getDay() + 6) % 7;
      var daysInMonth = new Date(year, month + 1, 0).getDate();
      var today = new Date();
      var todayIso = today.getFullYear() + '-' +
        String(today.getMonth() + 1).padStart(2, '0') + '-' +
        String(today.getDate()).padStart(2, '0');
      var monthName = first.toLocaleString('en-US', { month: 'long', year: 'numeric' });
      var html = '<div class="cal-header"><button type="button" id="cal-prev">&lt;</button>' +
        '<span>' + monthName + '</span><button type="button" id="cal-next">&gt;</button></div>' +
        '<div class="cal-grid">';
      ['M', 'T', 'W', 'T', 'F', 'S', 'S'].forEach(function (d) { html += '<span class="cal-dow">' + d + '</span>'; });
      for (var i = 0; i < startOffset; i++) html += '<span></span>';
      for (var d = 1; d <= daysInMonth; d++) {
        var iso = year + '-' + String(month + 1).padStart(2, '0') + '-' + String(d).padStart(2, '0');
        var cls = 'cal-day' + (iso === todayIso ? ' today' : '') + (iso === input.value ? ' selected' : '');
        html += '<button type="button" class="' + cls + '" data-date="' + iso + '">' + d + '</button>';
      }
      html += '</div>';
      cal.innerHTML = html;
      document.getElementById('cal-prev').addEventListener('click', function (e) {
        e.stopPropagation();
        ManualTime.renderCalendar(month === 0 ? year - 1 : year, month === 0 ? 11 : month - 1);
      });
      document.getElementById('cal-next').addEventListener('click', function (e) {
        e.stopPropagation();
        ManualTime.renderCalendar(month === 11 ? year + 1 : year, month === 11 ? 0 : month + 1);
      });
      cal.querySelectorAll('.cal-day').forEach(function (btn) {
        btn.addEventListener('click', function (e) {
          e.stopPropagation();
          input.value = btn.dataset.date;
          cal.hidden = true;
        });
      });
    },
    toggleComment: function () {
      var wrap = document.getElementById('manual-time-comment-wrap');
      wrap.hidden = !wrap.hidden;
      var btn = document.getElementById('manual-time-comment-btn');
      if (btn) btn.textContent = wrap.hidden ? 'Comment' : 'Hide comment';
      if (!wrap.hidden) document.getElementById('manual-time-comment').focus();
    },
    submit: function () {
      var self = this;
      var hours = parseInt(document.getElementById('manual-time-hours').value, 10) || 0;
      var minutes = parseInt(document.getElementById('manual-time-minutes').value, 10) || 0;
      var total = hours * 60 + minutes;
      var date = document.getElementById('manual-time-date').value;
      var comment = document.getElementById('manual-time-comment').value;
      if (total <= 0) { this.showError('Enter a duration greater than zero.'); return; }
      if (!date) { this.showError('Pick a date.'); return; }
      fetch('/api/time-entries', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          task_id: this.taskId,
          duration_minutes: total,
          date: date,
          comment: comment || null,
          source: 'manual',
        }),
      }).then(function (res) {
        if (res.ok) {
          self.close();
          if (modalTaskId()) { modalDirty = true; refreshModalTimeLog(); }
        } else {
          res.text().then(function (t) { self.showError(t || 'Could not save the time entry.'); });
        }
      }).catch(function () { self.showError('Could not save the time entry.'); });
    },
  };

  // ---------- Edit time entry dialog ----------

  var EditEntry = {
    entryId: null,
    originalDate: null,
    originalMinutes: null,
    originalComment: null,
    originalTaskId: null,
    open: function (entryId, data) {
      this.entryId = entryId;
      this.originalDate = data.date;
      this.originalMinutes = data.minutes;
      this.originalComment = data.comment || '';
      this.originalTaskId = data.task_id;
      document.getElementById('edit-entry-overlay').hidden = false;
      document.getElementById('edit-entry-date').value = data.date;
      document.getElementById('edit-entry-hours').value = Math.floor(data.minutes / 60);
      document.getElementById('edit-entry-minutes').value = data.minutes % 60;
      document.getElementById('edit-entry-comment').value = data.comment || '';
      document.getElementById('edit-entry-error').hidden = true;
      document.getElementById('edit-entry-date').focus();
    },
    close: function () {
      document.getElementById('edit-entry-overlay').hidden = true;
    },
    openCalendar: function () {
      var input = document.getElementById('edit-entry-date');
      var cal = document.getElementById('edit-entry-calendar');
      if (!cal.hidden) { cal.hidden = true; return; }
      var current = input.value ? new Date(input.value + 'T12:00:00') : new Date();
      this.renderCalendar(current.getFullYear(), current.getMonth());
      positionCalendar(input, cal);
      cal.hidden = false;
    },
    renderCalendar: function (year, month) {
      var cal = document.getElementById('edit-entry-calendar');
      var input = document.getElementById('edit-entry-date');
      var first = new Date(year, month, 1);
      var startOffset = (first.getDay() + 6) % 7;
      var daysInMonth = new Date(year, month + 1, 0).getDate();
      var today = new Date();
      var todayIso = today.getFullYear() + '-' +
        String(today.getMonth() + 1).padStart(2, '0') + '-' +
        String(today.getDate()).padStart(2, '0');
      var monthName = first.toLocaleString('en-US', { month: 'long', year: 'numeric' });
      var html = '<div class="cal-header"><button type="button" id="ecal-prev">&lt;</button>' +
        '<span>' + monthName + '</span><button type="button" id="ecal-next">&gt;</button></div>' +
        '<div class="cal-grid">';
      ['M', 'T', 'W', 'T', 'F', 'S', 'S'].forEach(function (d) { html += '<span class="cal-dow">' + d + '</span>'; });
      for (var i = 0; i < startOffset; i++) html += '<span></span>';
      for (var d = 1; d <= daysInMonth; d++) {
        var iso = year + '-' + String(month + 1).padStart(2, '0') + '-' + String(d).padStart(2, '0');
        var cls = 'cal-day' + (iso === todayIso ? ' today' : '') + (iso === input.value ? ' selected' : '');
        html += '<button type="button" class="' + cls + '" data-date="' + iso + '">' + d + '</button>';
      }
      html += '</div>';
      cal.innerHTML = html;
      document.getElementById('ecal-prev').addEventListener('click', function (e) {
        e.stopPropagation();
        EditEntry.renderCalendar(month === 0 ? year - 1 : year, month === 0 ? 11 : month - 1);
      });
      document.getElementById('ecal-next').addEventListener('click', function (e) {
        e.stopPropagation();
        EditEntry.renderCalendar(month === 11 ? year + 1 : year, month === 11 ? 0 : month + 1);
      });
      cal.querySelectorAll('.cal-day').forEach(function (btn) {
        btn.addEventListener('click', function (e) {
          e.stopPropagation();
          input.value = btn.dataset.date;
          cal.hidden = true;
        });
      });
    },
    submit: function () {
      var self = this;
      var hours = parseInt(document.getElementById('edit-entry-hours').value, 10) || 0;
      var minutes = parseInt(document.getElementById('edit-entry-minutes').value, 10) || 0;
      var total = hours * 60 + minutes;
      var date = document.getElementById('edit-entry-date').value;
      var comment = document.getElementById('edit-entry-comment').value;
      var err = document.getElementById('edit-entry-error');
      err.hidden = true;
      if (total <= 0) { err.textContent = 'Enter a duration greater than zero.'; err.hidden = false; return; }
      if (!date) { err.textContent = 'Pick a date.'; err.hidden = false; return; }
      // Only send fields that actually changed, mirroring the backend's PATCH shape.
      var patch = {};
      if (date !== this.originalDate) patch.date = date;
      if (total !== this.originalMinutes) patch.duration_minutes = total;
      if (comment !== this.originalComment) patch.comment = comment;
      if (Object.keys(patch).length === 0) { this.close(); return; }
      fetch('/api/time-entries/' + encodeURIComponent(this.entryId), {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(patch),
      }).then(function (res) {
        if (res.ok) {
          self.close();
          if (modalTaskId()) { modalDirty = true; refreshModalTimeLog(); }
          else window.location.reload();
        } else {
          res.text().then(function (t) {
            err.textContent = t || 'Could not save the entry.';
            err.hidden = false;
          });
        }
      }).catch(function () {
        err.textContent = 'Could not save the entry.';
        err.hidden = false;
      });
    },
    remove: function () {
      var self = this;
      if (!window.confirm('Delete this time entry?')) return;
      fetch('/api/time-entries/' + encodeURIComponent(this.entryId), { method: 'DELETE' })
        .then(function (res) {
          if (res.ok) {
            self.close();
            if (modalTaskId()) { modalDirty = true; refreshModalTimeLog(); }
            else window.location.reload();
          }
        });
    },
  };

  // ---------- time-entry edit delegation (modal log + /timer/log) ----------

  function initEntryEdit() {
    document.addEventListener('click', function (e) {
      var btn = e.target.closest('[data-edit-entry]');
      if (!btn) return;
      EditEntry.open(btn.getAttribute('data-edit-entry'), {
        date: btn.dataset.date,
        minutes: parseInt(btn.dataset.minutes, 10),
        comment: btn.dataset.comment,
        task_id: btn.dataset.taskId,
      });
    });

    // Manual-time dialog buttons (both pages carry this markup).
    var mtDate = document.getElementById('manual-time-date');
    if (mtDate) mtDate.addEventListener('click', function () { ManualTime.openCalendar(); });
    var mtCalBtn = document.getElementById('manual-time-calendar-btn');
    if (mtCalBtn) mtCalBtn.addEventListener('click', function (e) { e.stopPropagation(); ManualTime.openCalendar(); });
    var mtHours = document.getElementById('manual-time-hours');
    if (mtHours) mtHours.addEventListener('input', function () { ManualTime.updateDuration(); });
    var mtMinutes = document.getElementById('manual-time-minutes');
    if (mtMinutes) mtMinutes.addEventListener('input', function () { ManualTime.updateDuration(); });
    var mtCommentBtn = document.getElementById('manual-time-comment-btn');
    if (mtCommentBtn) mtCommentBtn.addEventListener('click', function () { ManualTime.toggleComment(); });
    var mtSubmit = document.getElementById('manual-time-submit');
    if (mtSubmit) mtSubmit.addEventListener('click', function () { ManualTime.submit(); });
    var mtErrorClose = document.getElementById('manual-time-error-close');
    if (mtErrorClose) mtErrorClose.addEventListener('click', function () { ManualTime.closeError(); });
    var futureOk = document.getElementById('future-time-ok');
    if (futureOk) futureOk.addEventListener('click', function () {
      document.getElementById('future-time-overlay').hidden = true;
    });

    // Edit-entry dialog buttons.
    var eeDate = document.getElementById('edit-entry-date');
    if (eeDate) eeDate.addEventListener('click', function () { EditEntry.openCalendar(); });
    var eeCalBtn = document.getElementById('edit-entry-calendar-btn');
    if (eeCalBtn) eeCalBtn.addEventListener('click', function (e) { e.stopPropagation(); EditEntry.openCalendar(); });
    var eeSave = document.getElementById('edit-entry-save');
    if (eeSave) eeSave.addEventListener('click', function () { EditEntry.submit(); });
    var eeDelete = document.getElementById('edit-entry-delete');
    if (eeDelete) eeDelete.addEventListener('click', function () { EditEntry.remove(); });

    // "Add time" buttons elsewhere (time log page header).
    document.querySelectorAll('[data-open-manual-time]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        ManualTime.open(btn.getAttribute('data-task-id') || null,
                        btn.getAttribute('data-task-name') || null);
      });
    });
  }

  // ---------- keyboard shortcuts ----------

  document.addEventListener('keydown', function (e) {
    // Never hijack typing inside inputs or the modal.
    if (e.target.closest('input, textarea, select, .task-modal, [contenteditable]')) return;
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    var key = e.key.toLowerCase();
    if (key === 'y') {
      var first = document.querySelector('.task-card');
      if (first) first.scrollIntoView({ behavior: 'smooth', block: 'center' });
    } else if (key === 't') {
      var modal = document.querySelector('.task-modal[data-task-id]');
      var id = modal ? modal.dataset.taskId : null;
      TimerUI.start('pomodoro', id);
    } else if (key === 'p') {
      TimerUI.stopClicked();
    } else if (key === 'e') {
      var modal2 = document.querySelector('.task-modal[data-task-id]');
      if (modal2) {
        document.getElementById('est-input').value = '';
        document.getElementById('estimate-dialog').hidden = false;
        document.getElementById('est-input').focus();
      }
    } else if (key === '?') {
      var shortcuts = document.getElementById('shortcuts-dialog');
      if (shortcuts) shortcuts.hidden = false;
    } else if (key === 'escape') {
      handleEscape();
    }
  });

  // Unified Escape: submenus, floating menus, dialogs, time dialogs, why
  // menu, then the task modal — innermost surface first.
  function handleEscape() {
    if (closeAllTmMenus()) return;
    if (hideFloatingMenus()) return;
    var open = document.querySelector('.dlg-overlay:not([hidden])');
    if (open) { open.hidden = true; return; }
    var mt = document.getElementById('manual-time-overlay');
    if (mt && !mt.hidden) { ManualTime.close(); return; }
    var ee = document.getElementById('edit-entry-overlay');
    if (ee && !ee.hidden) { EditEntry.close(); return; }
    var why = document.getElementById('why-stop-menu');
    if (why && !why.hidden) { TimerUI.closeWhyMenu(); return; }
    closeModal();
  }

  // ---------- timer log page ----------

  function initTimerLogPage() {
    var params = new URLSearchParams(window.location.search);
    var period = params.get('period') || 'week';
    var btn = document.getElementById('timer-log-period');
    if (btn) {
      btn.textContent = 'Period: ' + period;
      btn.addEventListener('click', function () {
        var order = ['day', 'week', 'month'];
        var next = order[(order.indexOf(period) + 1) % order.length];
        params.set('period', next);
        window.location.search = params.toString();
      });
    }
  }

  // ---------- timer statistics page ----------

  function initTimerStatsPage() {
    var svg = document.getElementById('stats-chart');
    if (!svg || !svg.dataset.bars) return;
    var bars = [];
    try { bars = JSON.parse(svg.dataset.bars); } catch (e) { return; }
    renderBarChart(svg, bars);
  }

  function renderBarChart(svg, bars) {
    var W = 720, H = 280, padL = 44, padB = 30, padT = 14;
    var max = 1;
    bars.forEach(function (b) { if (b.minutes > max) max = b.minutes; });
    var innerW = W - padL - 10;
    var innerH = H - padT - padB;
    var slot = innerW / Math.max(1, bars.length);
    var bw = Math.min(40, slot * 0.55);
    var html = '';
    // Gridlines + y labels (minutes).
    for (var g = 0; g <= 4; g++) {
      var val = Math.round(max * g / 4);
      var y = padT + innerH - (innerH * g / 4);
      html += '<line x1="' + padL + '" y1="' + y + '" x2="' + W + '" y2="' + y +
        '" stroke="#e3e3e3"/>' +
        '<text x="' + (padL - 6) + '" y="' + (y + 4) + '" text-anchor="end" font-size="11" fill="#777">' +
        val + '</text>';
    }
    bars.forEach(function (b, i) {
      var h = innerH * (b.minutes / max);
      var x = padL + slot * i + (slot - bw) / 2;
      var y = padT + innerH - h;
      html += '<rect x="' + x.toFixed(1) + '" y="' + y.toFixed(1) + '" width="' + bw.toFixed(1) +
        '" height="' + h.toFixed(1) + '" fill="#2f7cf6" rx="2">' +
        '<title>' + escapeHtml(b.label) + ': ' + b.minutes + ' min</title></rect>' +
        '<text x="' + (padL + slot * i + slot / 2).toFixed(1) + '" y="' + (H - 10) +
        '" text-anchor="middle" font-size="11" fill="#777">' + escapeHtml(b.label) + '</text>';
    });
    svg.setAttribute('viewBox', '0 0 ' + W + ' ' + H);
    svg.innerHTML = html;
  }

  // ---------- boot ----------

  function initBoard() {
    initTaskSortable();
    initColumnSortable();
    applyCollapsedColumns();
    initAddTask();
    initBoardMenus();

    // Click a card to open its modal. Drags, and clicks on interactive
    // elements inside a card, are ignored.
    document.addEventListener('click', function (e) {
      if (dragging) return;
      if (e.target.closest('button, a, input, select, textarea, form, .task-modal, .timer-popup, .why-stop-menu, .menu-pop, .tm-menu, .dlg-overlay')) return;
      var card = e.target.closest('.task-card');
      if (card && card.dataset.taskId) openModal(card.dataset.taskId);
    });

    // Enter on a focused card opens it (cards are tabindex=0).
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Enter' && e.target.classList &&
          e.target.classList.contains('task-card')) {
        openModal(e.target.dataset.taskId);
      }
    });
  }

  document.addEventListener('DOMContentLoaded', function () {
    if (document.querySelector('.board-wrap')) initBoard();
    initEntryEdit();
    initTimerLogPage();
    initTimerStatsPage();
    TimerUI.init();
  });
  // In case app.js runs after DOMContentLoaded (defer ordering):
  if (document.readyState !== 'loading') {
    if (document.querySelector('.board-wrap')) initBoard();
    initEntryEdit();
    initTimerLogPage();
    initTimerStatsPage();
    TimerUI.init();
  }

  // Exposed for inline handlers and debugging.
  window.closeModal = closeModal;
  window.TimerUI = TimerUI;
  window.ManualTime = ManualTime;
  window.EditEntry = EditEntry;
})();
