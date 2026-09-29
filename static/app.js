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

    // Card right-click menu (KanbanFlow parity, KF-050).
    document.addEventListener('contextmenu', function (e) {
      var card = e.target.closest('.task-card');
      if (card) {
        e.preventDefault();
        openCardMenu(card, e.clientX, e.clientY);
        return;
      }
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

    // ---------- card context menu (KF-050) ----------
    var cardMenuCard = null;

    function cardMenuTask() {
      if (!cardMenuCard) return null;
      return {
        id: cardMenuCard.dataset.taskId,
        name: cardMenuCard.dataset.taskName || '',
        groupingDate: cardMenuCard.dataset.groupingDate || '',
      };
    }

    function openCardMenu(card, x, y) {
      hideFloatingMenus();
      cardMenuCard = card;
      placeMenu(document.getElementById('card-ctx-menu'), x, y);
    }

    function hideCardSubmenu() {
      var sub = document.getElementById('card-ctx-submenu');
      if (sub) { sub.hidden = true; sub.innerHTML = ''; }
    }

    function openCardSubmenu(btn, buildItems) {
      var sub = document.getElementById('card-ctx-submenu');
      sub.innerHTML = '';
      buildItems(sub);
      sub.hidden = false;
      var r = btn.getBoundingClientRect();
      placeMenu(sub, r.right + 2, r.top - 6);
      // Flip to the left when the submenu would run off the viewport.
      var sr = sub.getBoundingClientRect();
      if (sr.right > window.innerWidth - 4) {
        placeMenu(sub, r.left - sr.width - 2, r.top - 6);
      }
    }

    function cardSubmenuButton(label, subAct, extra) {
      var b = document.createElement('button');
      b.type = 'button';
      b.textContent = label;
      b.setAttribute('data-card-sub', subAct);
      if (extra) {
        Object.keys(extra).forEach(function (k) { b.setAttribute(k, extra[k]); });
      }
      return b;
    }

    document.getElementById('card-ctx-menu').addEventListener('click', function (e) {
      var btn = e.target.closest('[data-card-act]');
      if (!btn || !cardMenuCard) return;
      var act = btn.getAttribute('data-card-act');
      var task = cardMenuTask();
      if (act === 'timer') {
        openCardSubmenu(btn, function (sub) {
          sub.appendChild(cardSubmenuButton('Start timer', 'timer-start'));
          sub.appendChild(cardSubmenuButton('Select in timer', 'timer-select'));
        });
      } else if (act === 'move') {
        openCardSubmenu(btn, function (sub) {
          document.querySelectorAll('.columnHeader[data-column-id]').forEach(function (th) {
            sub.appendChild(cardSubmenuButton(th.dataset.columnName || 'Column', 'move-col',
              { 'data-column-id': th.dataset.columnId }));
          });
        });
      } else if (act === 'color') {
        openCardSubmenu(btn, function (sub) {
          var src = document.getElementById('board-colors');
          if (src) {
            Array.prototype.forEach.call(src.querySelectorAll('span[data-id]'), function (s) {
              var b = cardSubmenuButton(s.dataset.label || s.dataset.value, 'color',
                { 'data-color-id': s.dataset.id, 'data-color-value': s.dataset.value });
              b.style.borderLeft = '0.9rem solid ' + (s.dataset.bg || '#fff');
              sub.appendChild(b);
            });
          }
        });
      } else {
        hideCardSubmenu();
        hideFloatingMenus();
        if (act === 'grouping-date') openGroupingDateDialog(task);
        else if (act === 'assign-members') openMembersDialog(task ? task.id : null);
        else if (act === 'copy-here') copyCardHere(task);
        else if (act === 'task-url') copyTaskUrl(task ? task.id : null);
        else if (act === 'delete') deleteCardTask(task);
      }
    });

    document.getElementById('card-ctx-submenu').addEventListener('click', function (e) {
      var btn = e.target.closest('[data-card-sub]');
      if (!btn || !cardMenuCard) return;
      var sub = btn.getAttribute('data-card-sub');
      var task = cardMenuTask();
      hideCardSubmenu();
      hideFloatingMenus();
      if (sub === 'timer-start') {
        if (typeof TimerUI !== 'undefined' && TimerUI.startForTask) {
          TimerUI.startForTask(task.id, task.name);
        } else {
          toast('Timer is not available on this page.');
        }
      } else if (sub === 'timer-select') {
        selectTaskInTimer(task.id);
      } else if (sub === 'move-col') {
        api('/api/tasks/' + encodeURIComponent(task.id) + '/move', 'PATCH',
            { column_id: btn.getAttribute('data-column-id') })
          .then(function (res) {
            if (res.ok) window.location.reload();
            else toast('Could not move task.');
          });
      } else if (sub === 'color') {
        api('/api/tasks/' + encodeURIComponent(task.id), 'PATCH',
            { color_id: btn.getAttribute('data-color-id') })
          .then(function (res) {
            if (!res.ok) { toast('Could not change color.'); return; }
            var value = btn.getAttribute('data-color-value');
            var oldVal = cardMenuCard.dataset.colorValue;
            if (oldVal) {
              cardMenuCard.classList.remove('taskColor-' + oldVal, 'taskBorderColor-' + oldVal);
            }
            if (value) {
              cardMenuCard.classList.add('taskColor-' + value, 'taskBorderColor-' + value);
              cardMenuCard.dataset.colorValue = value;
            }
          });
      }
    });

    // Escape closes the card menu and its submenu.
    document.addEventListener('keydown', function (e) {
      if (e.key !== 'Escape') return;
      var menu = document.getElementById('card-ctx-menu');
      var sub = document.getElementById('card-ctx-submenu');
      if ((menu && !menu.hidden) || (sub && !sub.hidden)) {
        hideCardSubmenu();
        hideFloatingMenus();
      }
    });

    function copyCardHere(task) {
      if (!task) return;
      var list = cardMenuCard.closest('.task-list');
      if (!list) { toast('Could not copy task.'); return; }
      var src = document.querySelector(
        '#board-colors span[data-value="' + cssEscape(cardMenuCard.dataset.colorValue) + '"]');
      api('/api/tasks', 'POST', {
        column_id: list.dataset.columnId,
        swimlane_id: list.dataset.swimlaneId || null,
        name: task.name + ' (copy)',
        color_id: src ? src.dataset.id : null,
      }).then(function (res) {
        if (res.ok) window.location.reload();
        else toast('Could not copy task.');
      });
    }

    function deleteCardTask(task) {
      if (!task) return;
      if (!window.confirm('Delete "' + task.name + '"?')) return;
      api('/api/tasks/' + encodeURIComponent(task.id), 'DELETE')
        .then(function (res) {
          if (!res.ok) { toast('Could not delete task.'); return; }
          if (cardMenuCard && cardMenuCard.parentNode) cardMenuCard.remove();
          cardMenuCard = null;
        });
    }

    function selectTaskInTimer(taskId) {
      if (typeof TimerUI === 'undefined' || !TimerUI.togglePopup) {
        toast('Timer is not available on this page.');
        return;
      }
      TimerUI.togglePopup();
      // The popup renders the task select when idle; pick our task there.
      window.setTimeout(function () {
        var sel = document.getElementById('tt-task');
        if (sel) {
          TimerUI.fillTaskOptions(sel);
          sel.value = taskId;
          toast('Task selected in timer.');
        } else {
          toast('Timer is running; stop it first to select a task.');
        }
      }, 50);
    }

    // ---------- grouping date dialog (card menu) ----------
    var groupingDateTaskId = null;

    function openGroupingDateDialog(task) {
      groupingDateTaskId = task ? task.id : null;
      var input = document.getElementById('gd-input');
      if (input) input.value = task ? task.groupingDate : '';
      document.getElementById('grouping-date-dialog').hidden = false;
      if (input) input.focus();
    }

    document.getElementById('gd-save').addEventListener('click', function () {
      if (!groupingDateTaskId) return;
      var value = document.getElementById('gd-input').value || null;
      api('/api/tasks/' + encodeURIComponent(groupingDateTaskId), 'PATCH', { grouping_date: value })
        .then(function (res) {
          if (!res.ok) { toast('Could not save grouping date.'); return; }
          document.getElementById('grouping-date-dialog').hidden = true;
          if (cardMenuCard) cardMenuCard.dataset.groupingDate = value || '';
          toast('Grouping date saved.');
        });
    });

    document.getElementById('gd-clear').addEventListener('click', function () {
      document.getElementById('gd-input').value = '';
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

  // KF-043: the six "Task properties to display on board" dropdowns. Values
  // live in the column's opaque config_json bag, surfaced to the dialog via
  // the header's data-column-config attribute.
  var COLUMN_PROP_DEFAULTS = {
    description: 'hide',
    labels: 'hide',
    subtasks: 'hide',
    due_dates: 'active_7d',
    created: 'hide',
    added: 'hide',
  };

  function columnPropConfig(th) {
    var cfg = {};
    if (th && th.dataset.columnConfig) {
      try { cfg = JSON.parse(th.dataset.columnConfig) || {}; } catch (e) { cfg = {}; }
    }
    var out = {};
    Object.keys(COLUMN_PROP_DEFAULTS).forEach(function (key) {
      out[key] = cfg['prop_' + key] || COLUMN_PROP_DEFAULTS[key];
    });
    return out;
  }

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
    var props = columnPropConfig(th);
    document.getElementById('ec-prop-description').value = props.description;
    document.getElementById('ec-prop-labels').value = props.labels;
    document.getElementById('ec-prop-subtasks').value = props.subtasks;
    document.getElementById('ec-prop-due-dates').value = props.due_dates;
    document.getElementById('ec-prop-created').value = props.created;
    document.getElementById('ec-prop-added').value = props.added;
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
      // KF-043: per-property display dropdowns, persisted into the
      // column's opaque config_json bag.
      prop_description: document.getElementById('ec-prop-description').value,
      prop_labels: document.getElementById('ec-prop-labels').value,
      prop_subtasks: document.getElementById('ec-prop-subtasks').value,
      prop_due_dates: document.getElementById('ec-prop-due-dates').value,
      prop_created: document.getElementById('ec-prop-created').value,
      prop_added: document.getElementById('ec-prop-added').value,
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
    } else if (act === 'add-description') {
      var desc = document.getElementById('modal-description');
      if (desc) { desc.focus(); desc.scrollIntoView({ behavior: 'smooth', block: 'center' }); }
    } else if (act === 'add-member') {
      openMembersDialog(id);
    } else if (act === 'add-label') {
      // Labels dialog is tracked as a separate parity defect.
      toast('Labels are not supported yet.');
    } else if (act === 'add-subtask') {
      var subInput = document.getElementById('modal-subtask-input');
      if (subInput) { subInput.focus(); subInput.scrollIntoView({ behavior: 'smooth', block: 'center' }); }
    } else if (act === 'add-duedate') {
      // Due-date dialog is tracked as a separate parity defect.
      toast('Due dates are not supported yet.');
    } else if (act === 'add-comment') {
      // Comments are tracked as a separate parity defect.
      toast('Comments are not supported yet.');
    } else if (act === 'add-attachment') {
      // Attachments are tracked as a separate parity defect.
      toast('Attachments are not supported yet.');
    } else if (act === 'add-relation') {
      // Relations are tracked as a separate parity defect.
      toast('Relations are not supported yet.');
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
    wireSubtasks();
  }

  // ---------- Subtasks (KF-058) ----------
  //
  // Wired per modal open: Enter in the "Add subtask..." row creates one,
  // checkboxes toggle done, clicking a name edits it inline, and the
  // x-button deletes it.

  function wireSubtasks() {
    var wrap = document.getElementById('modal-subtasks');
    var input = document.getElementById('modal-subtask-input');
    if (!wrap || !input) return;
    var taskId = modalTaskId();
    if (!taskId) return;

    function subtaskRow(sub) {
      var div = document.createElement('div');
      div.className = 'tm-subtask' + (sub.done ? ' tm-subtask-done' : '');
      div.dataset.subtaskId = sub.id;
      var check = document.createElement('input');
      check.type = 'checkbox';
      check.className = 'tm-subtask-check';
      check.checked = !!sub.done;
      check.setAttribute('aria-label', 'Mark subtask done');
      var name = document.createElement('span');
      name.className = 'tm-subtask-name';
      name.tabIndex = 0;
      name.title = 'Click to edit';
      name.textContent = sub.name;
      var del = document.createElement('button');
      del.type = 'button';
      del.className = 'tm-subtask-del';
      del.setAttribute('aria-label', 'Delete subtask');
      del.innerHTML = '&times;';
      div.appendChild(check);
      div.appendChild(name);
      div.appendChild(del);
      return div;
    }

    function addSubtask() {
      var name = input.value.trim();
      if (!name) return;
      api('/api/tasks/' + encodeURIComponent(taskId) + '/subtasks', 'POST', { name: name })
        .then(function (res) { return res.ok ? res.json() : null; })
        .then(function (sub) {
          if (!sub) { toast('Could not add subtask.'); return; }
          wrap.appendChild(subtaskRow(sub));
          input.value = '';
          input.focus();
          modalDirty = true;
        });
    }

    input.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') { e.preventDefault(); addSubtask(); }
    });

    wrap.addEventListener('change', function (e) {
      var check = e.target.closest('.tm-subtask-check');
      if (!check) return;
      var row = check.closest('.tm-subtask');
      api('/api/tasks/' + encodeURIComponent(taskId) + '/subtasks/' +
          encodeURIComponent(row.dataset.subtaskId), 'PATCH', { done: check.checked })
        .then(function (res) {
          if (!res.ok) { toast('Could not update subtask.'); check.checked = !check.checked; return; }
          row.classList.toggle('tm-subtask-done', check.checked);
          modalDirty = true;
        });
    });

    wrap.addEventListener('click', function (e) {
      var del = e.target.closest('.tm-subtask-del');
      if (del) {
        var delRow = del.closest('.tm-subtask');
        api('/api/tasks/' + encodeURIComponent(taskId) + '/subtasks/' +
            encodeURIComponent(delRow.dataset.subtaskId), 'DELETE')
          .then(function (res) {
            if (!res.ok) { toast('Could not delete subtask.'); return; }
            delRow.remove();
            modalDirty = true;
          });
        return;
      }
      var name = e.target.closest('.tm-subtask-name');
      if (name) startSubtaskEdit(name);
    });

    wrap.addEventListener('keydown', function (e) {
      var name = e.target.closest('.tm-subtask-name');
      if (name && e.key === 'Enter') { e.preventDefault(); startSubtaskEdit(name); }
    });

    function startSubtaskEdit(nameEl) {
      var row = nameEl.closest('.tm-subtask');
      if (row.querySelector('.tm-subtask-edit')) return;
      var current = nameEl.textContent;
      var edit = document.createElement('input');
      edit.type = 'text';
      edit.className = 'tm-subtask-edit';
      edit.value = current;
      edit.maxLength = 200;
      edit.setAttribute('aria-label', 'Edit subtask');
      nameEl.replaceWith(edit);
      edit.focus();
      edit.select();
      var settled = false;
      function finish(save) {
        if (settled) return;
        settled = true;
        var val = edit.value.trim();
        if (!save || !val || val === current) { edit.replaceWith(nameEl); return; }
        api('/api/tasks/' + encodeURIComponent(taskId) + '/subtasks/' +
            encodeURIComponent(row.dataset.subtaskId), 'PATCH', { name: val })
          .then(function (res) { return res.ok ? res.json() : null; })
          .then(function (sub) {
            if (sub) { nameEl.textContent = sub.name; modalDirty = true; }
            else toast('Could not rename subtask.');
            edit.replaceWith(nameEl);
          });
      }
      edit.addEventListener('keydown', function (ev) {
        if (ev.key === 'Enter') { ev.preventDefault(); finish(true); }
        else if (ev.key === 'Escape') { ev.preventDefault(); finish(false); }
      });
      edit.addEventListener('blur', function () { finish(true); });
    }
  }

  // ---------- Members dialog (KF-060) ----------
  //
  // Shared by the task-modal Add menu and the card context menu. Shows the
  // board roster with a Search... filter; clicking a row toggles that
  // member's assignment on the task (persisted via PATCH /api/tasks/:id).
  // The gear navigates to Settings, matching the header gear convention.

  var membersDialogTaskId = null;
  var membersRoster = [];
  var membersAssigned = [];

  function initMembersDialog() {
    var dlg = document.getElementById('members-dialog');
    if (!dlg || dlg._wired) return;
    dlg._wired = true;
    document.getElementById('members-search').addEventListener('input', function (e) {
      renderMembersList(e.target.value);
    });
    document.getElementById('members-list').addEventListener('click', function (e) {
      var row = e.target.closest('.member-row[data-member-id]');
      if (row) toggleMemberAssignment(row.dataset.memberId);
    });
    document.getElementById('members-gear').addEventListener('click', function () {
      window.location.href = '/settings';
    });
  }

  function openMembersDialog(taskId) {
    initMembersDialog();
    membersDialogTaskId = taskId || null;
    membersRoster = [];
    membersAssigned = [];
    var search = document.getElementById('members-search');
    var list = document.getElementById('members-list');
    search.value = '';
    list.innerHTML = '<div class="member-row">Loading…</div>';
    document.getElementById('members-dialog').hidden = false;
    search.focus();
    var rosterP = fetchJson('/api/members').catch(function () { return []; });
    var taskP = membersDialogTaskId
      ? fetchJson('/api/tasks/' + encodeURIComponent(membersDialogTaskId))
          .catch(function () { return null; })
      : Promise.resolve(null);
    var wanted = membersDialogTaskId;
    Promise.all([rosterP, taskP]).then(function (results) {
      // Ignore stale responses if the dialog moved on to another task.
      if (wanted !== membersDialogTaskId ||
          document.getElementById('members-dialog').hidden) return;
      membersRoster = results[0] || [];
      membersAssigned = (results[1] && results[1].member_ids) || [];
      renderMembersList('');
    });
  }

  function renderMembersList(filter) {
    var list = document.getElementById('members-list');
    list.innerHTML = '';
    var q = (filter || '').trim().toLowerCase();
    var shown = 0;
    membersRoster.forEach(function (m) {
      if (q && m.username.toLowerCase().indexOf(q) === -1) return;
      shown++;
      var row = document.createElement('button');
      row.type = 'button';
      row.className = 'member-row';
      row.setAttribute('role', 'option');
      var isAssigned = membersAssigned.indexOf(m.id) !== -1;
      row.setAttribute('aria-selected', isAssigned ? 'true' : 'false');
      row.dataset.memberId = m.id;
      var dot = document.createElement('span');
      dot.className = 'member-dot';
      dot.setAttribute('aria-hidden', 'true');
      dot.innerHTML = '&#9679;';
      var name = document.createElement('span');
      name.className = 'member-name';
      name.textContent = m.username;
      row.appendChild(dot);
      row.appendChild(name);
      if (isAssigned) {
        var check = document.createElement('span');
        check.className = 'member-check';
        check.setAttribute('aria-hidden', 'true');
        check.innerHTML = '&#10003;';
        row.appendChild(check);
      }
      list.appendChild(row);
    });
    if (!shown) {
      var empty = document.createElement('div');
      empty.className = 'member-row';
      empty.textContent = membersRoster.length ? 'No members match.' : 'No members on this board.';
      list.appendChild(empty);
    }
  }

  function toggleMemberAssignment(memberId) {
    if (!membersDialogTaskId) { toast('Open a task to assign members.'); return; }
    var next = membersAssigned.slice();
    var idx = next.indexOf(memberId);
    if (idx === -1) next.push(memberId);
    else next.splice(idx, 1);
    api('/api/tasks/' + encodeURIComponent(membersDialogTaskId), 'PATCH', { member_ids: next })
      .then(function (res) {
        if (!res.ok) { toast('Could not update members.'); return; }
        membersAssigned = next;
        renderMembersList(document.getElementById('members-search').value);
        // Refresh the modal body row when the dialog was opened for the
        // modal's task.
        if (modalTaskId() && modalTaskId() === membersDialogTaskId) {
          modalDirty = true;
          openModal(membersDialogTaskId);
          // Keep the dialog on top after the modal re-renders.
          document.getElementById('members-dialog').hidden = false;
          document.getElementById('members-search').focus();
        }
      });
  }

  // ---------- Timer settings modal (KF-074, KF-079) ----------

  var TimerSettings = {
    settings: null,

    open: function () {
      var self = this;
      fetch('/api/settings', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then(function (r) { if (!r.ok) throw new Error('HTTP ' + r.status); return r.json(); })
        .then(function (s) {
          self.settings = s;
          self.populate();
          self.showTab('general');
          document.getElementById('timer-settings-overlay').hidden = false;
          self.wireSliders();
        })
        .catch(function (e) { toast('Could not load timer settings: ' + e.message); });
    },

    wireSliders: function () {
      var av = document.getElementById('ts-alarm-vol');
      var avv = document.getElementById('ts-alarm-vol-val');
      if (av && !av._wired) {
        av._wired = true;
        av.addEventListener('input', function () { avv.textContent = av.value + '%'; });
      }
      var pv = document.getElementById('ts-points-vol');
      var pvv = document.getElementById('ts-points-vol-val');
      if (pv && !pv._wired) {
        pv._wired = true;
        pv.addEventListener('input', function () { pvv.textContent = pv.value + '%'; });
      }
    },

    close: function () {
      document.getElementById('timer-settings-overlay').hidden = true;
    },

    showTab: function (name) {
      var tabs = document.querySelectorAll('.ts-tab');
      for (var i = 0; i < tabs.length; i++) {
        tabs[i].classList.toggle('active', tabs[i].getAttribute('data-tstab') === name);
      }
      var panes = document.querySelectorAll('.ts-pane');
      for (var j = 0; j < panes.length; j++) {
        panes[j].hidden = panes[j].id !== 'tstab-' + name;
      }
    },

    populate: function () {
      var s = this.settings;
      if (!s) return;
      setSel('ts-work-time', s.pomodoro_minutes);
      setSel('ts-short-break', s.short_break_minutes);
      setSel('ts-long-break', s.long_break_minutes);
      setSel('ts-long-interval', s.long_break_every);
      this.setToggle('ts-pip', !!s.pip_enabled);
      this.setToggle('ts-pip2', !!s.pip_enabled);
      this.renderReasons();
      setSel('ts-ticking', s.ticking_mode || 'never');
      setSel('ts-alarm-sound', s.alarm_sound || 'bell');
      var av = document.getElementById('ts-alarm-vol');
      av.value = s.alarm_volume != null ? s.alarm_volume : 70;
      document.getElementById('ts-alarm-vol-val').textContent = av.value + '%';
      var pv = document.getElementById('ts-points-vol');
      pv.value = s.points_volume != null ? s.points_volume : 70;
      document.getElementById('ts-points-vol-val').textContent = pv.value + '%';
      this.setToggle('ts-sounds', s.sounds_enabled !== false);
      function setSel(id, v) {
        var el = document.getElementById(id);
        if (!el) return;
        var str = String(v);
        var found = false;
        for (var i = 0; i < el.options.length; i++) {
          if (el.options[i].value === str) { found = true; break; }
        }
        if (found) el.value = str;
      }
    },

    setToggle: function (id, on) {
      var el = document.getElementById(id);
      if (el) el.setAttribute('aria-checked', on ? 'true' : 'false');
    },

    getToggle: function (id) {
      var el = document.getElementById(id);
      return el && el.getAttribute('aria-checked') === 'true';
    },

    togglePip: function () {
      var on = !this.getToggle('ts-pip');
      this.setToggle('ts-pip', on);
      this.setToggle('ts-pip2', on);
    },

    toggleSounds: function () {
      this.setToggle('ts-sounds', !this.getToggle('ts-sounds'));
    },

    renderReasons: function () {
      var list = document.getElementById('ts-reasons-list');
      if (!list || !this.settings) return;
      var reasons = this.settings.interrupt_reasons || [];
      var html = '';
      for (var i = 0; i < reasons.length; i++) {
        html += '<div class="ts-reason-row"><span>' + escapeHtml(reasons[i]) + '</span>' +
          '<button type="button" onclick="TimerSettings.removeReason(' + i + ')" aria-label="Remove">&times;</button></div>';
      }
      list.innerHTML = html || '<p class="settings-hint">No reasons yet.</p>';
    },

    addReason: function () {
      var input = document.getElementById('ts-reason-new');
      var v = input.value.trim();
      if (!v || !this.settings) return;
      this.settings.interrupt_reasons = this.settings.interrupt_reasons || [];
      this.settings.interrupt_reasons.push(v);
      input.value = '';
      this.renderReasons();
    },

    removeReason: function (i) {
      if (!this.settings || !this.settings.interrupt_reasons) return;
      this.settings.interrupt_reasons.splice(i, 1);
      this.renderReasons();
    },

    addActivity: function () {
      toast('Break activities are coming soon.');
    },

    testSound: function () {
      var sel = document.getElementById('ts-alarm-sound');
      var vol = document.getElementById('ts-alarm-vol');
      playAlarmSound(sel ? sel.value : 'bell', vol ? parseInt(vol.value, 10) : 70);
    },

    save: function () {
      var self = this;
      var saved = document.getElementById('ts-saved');
      saved.hidden = true;
      var body = {
        pomodoro_minutes: parseInt(document.getElementById('ts-work-time').value, 10),
        short_break_minutes: parseInt(document.getElementById('ts-short-break').value, 10),
        long_break_minutes: parseInt(document.getElementById('ts-long-break').value, 10),
        long_break_every: parseInt(document.getElementById('ts-long-interval').value, 10),
        pip_enabled: this.getToggle('ts-pip'),
        ticking_mode: document.getElementById('ts-ticking').value,
        alarm_sound: document.getElementById('ts-alarm-sound').value,
        alarm_volume: parseInt(document.getElementById('ts-alarm-vol').value, 10),
        points_volume: parseInt(document.getElementById('ts-points-vol').value, 10),
        sounds_enabled: this.getToggle('ts-sounds'),
        interrupt_reasons: this.settings ? this.settings.interrupt_reasons : undefined
      };
      fetch('/api/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify(body)
      })
        .then(function (r) {
          if (!r.ok) return r.text().then(function (t) { throw new Error(t || ('HTTP ' + r.status)); });
          return r.json();
        })
        .then(function (s) {
          self.settings = s;
          if (window.TimerUI) TimerUI.settings = s;
          saved.hidden = false;
          setTimeout(function () { saved.hidden = true; }, 2000);
        })
        .catch(function (e) { toast('Could not save timer settings: ' + e.message); });
    }
  };

  // Distinct Web-Audio alarm sounds (KF-079): bell, chime, beeps, blip,
  // glass, microwave, egg_timer, grandpa_clock, melodic.
  function playAlarmSound(name, volumePct) {
    try {
      var Ctx = window.AudioContext || window.webkitAudioContext;
      if (!Ctx) return;
      var ctx = new Ctx();
      var vol = Math.max(0, Math.min(100, volumePct == null ? 70 : volumePct)) / 100;
      var t0 = ctx.currentTime + 0.02;
      function beep(freq, at, dur, type, peak) {
        var o = ctx.createOscillator();
        var g = ctx.createGain();
        o.connect(g); g.connect(ctx.destination);
        o.type = type || 'sine';
        o.frequency.value = freq;
        var p = (peak == null ? 0.5 : peak) * vol;
        g.gain.setValueAtTime(0.001, t0 + at);
        g.gain.exponentialRampToValueAtTime(Math.max(0.001, p), t0 + at + 0.02);
        g.gain.exponentialRampToValueAtTime(0.001, t0 + at + dur);
        o.start(t0 + at); o.stop(t0 + at + dur + 0.05);
      }
      switch (name) {
        case 'chime':
          beep(1318, 0, 0.9, 'sine'); beep(1760, 0.25, 1.0, 'sine'); break;
        case 'beeps':
          beep(880, 0, 0.18, 'square', 0.3); beep(880, 0.25, 0.18, 'square', 0.3); beep(880, 0.5, 0.3, 'square', 0.3); break;
        case 'blip':
          beep(1200, 0, 0.12, 'sine', 0.4); break;
        case 'glass':
          beep(2093, 0, 1.2, 'sine', 0.35); beep(2637, 0.05, 1.0, 'sine', 0.2); break;
        case 'microwave':
          beep(660, 0, 0.4, 'square', 0.25); beep(660, 0.5, 0.4, 'square', 0.25); beep(660, 1.0, 0.6, 'square', 0.25); break;
        case 'egg_timer':
          for (var i = 0; i < 6; i++) beep(1568, i * 0.18, 0.12, 'triangle', 0.4); break;
        case 'grandpa_clock':
          beep(196, 0, 0.8, 'sine', 0.6); beep(147, 0.9, 1.0, 'sine', 0.6); break;
        case 'melodic':
          beep(523, 0, 0.3, 'sine'); beep(659, 0.3, 0.3, 'sine'); beep(784, 0.6, 0.5, 'sine'); break;
        case 'bell':
        default:
          beep(880, 0, 1.2, 'sine', 0.5); beep(1320, 0.02, 0.9, 'sine', 0.25); break;
      }
    } catch (e) { /* audio is best-effort */ }
  }

  // ---------- Timer UI (header pill + popup) ----------

  var TimerUI = {
    settings: null,
    state: null,
    initialized: false,
    pollHandle: null,
    tickHandle: null,
    lastStatus: null,
    // KF-006: the session that just ran to zero, until the user takes or
    // skips the break. { mode: 'pomodoro'|'short_break'|'long_break', at }.
    finished: null,
    // why-stop flow
    whyOriginal: null,
    whySessionId: null,
    whyEntryId: null,
    whyMode: null,
    whyTaskId: null,
    whySeconds: 0,
    whyStartWall: null,
    whyTaskName: 'Pomodoro',

    init: function () {
      // The bootstrap block below calls init() both immediately (when the
      // deferred script runs after parsing) and on DOMContentLoaded. Guard
      // so listeners are attached exactly once: a doubled pill listener
      // toggles the popup open then shut on a single click.
      if (this.initialized) return;
      this.initialized = true;
      var self = this;
      fetch('/api/timer/settings', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
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
      fetch('/api/timer/status', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
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
        startedAt: status.started_at,
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

    // KF-098: idle pill shows the configured duration with a green play
    // triangle (pomodoro tab) or a red stop square + 00:00 (stopwatch tab);
    // a running session shows a red stop square + live countdown/count-up.
    renderPill: function () {
      var pill = document.getElementById('timer-pill');
      if (!pill) return;
      // The pill is the always-visible timer dropdown control; never leave
      // it hidden, and update the icon/label spans in place so the pill's
      // own click listener survives re-renders.
      pill.hidden = false;
      var s = this.state;
      var icon = document.getElementById('timer-pill-icon');
      var timeEl = document.getElementById('timer-pill-time');
      var active = !!(s && s.phase !== 'idle');
      var glyph = '&#9654;';
      var cls = 'timer-pill-icon timer-pill-play';
      var label = '--:--';
      if (!active) {
        if (this.currentModeTab === 'stopwatch') {
          glyph = '&#9632;';
          cls = 'timer-pill-icon timer-pill-stop';
          label = '00:00';
        } else {
          var mins = this.settings && this.settings.pomodoro_minutes
            ? this.settings.pomodoro_minutes : 25;
          label = this.fmt(mins * 60);
        }
      } else if (s.mode === 'stopwatch') {
        glyph = '&#9632;';
        cls = 'timer-pill-icon timer-pill-stop';
        label = this.fmt(s.startedAt
          ? Math.max(0, Math.floor(Date.now() / 1000) - s.startedAt) : 0);
      } else {
        glyph = '&#9632;';
        cls = 'timer-pill-icon timer-pill-stop';
        label = this.fmt(s.remainingSeconds || 0);
      }
      if (icon) { icon.innerHTML = glyph; icon.className = cls; }
      if (timeEl) timeEl.textContent = label;
      // KF-027: the running state must be visibly distinct from idle.
      pill.classList.toggle('running', active);
      var st = document.getElementById('timer-status');
      if (st) {
        if (this.state && this.state.phase !== 'idle') {
          var label2 = this.state.mode === 'pomodoro' ? 'Focus' :
                      this.state.mode === 'stopwatch' ? 'Stopwatch' : 'Break';
          st.textContent = label2 + ' — ' +
            (this.state.taskName || 'Pomodoro') + ' ' + this.fmt(this.state.remainingSeconds);
        } else {
          st.textContent = '';
        }
      }
    },

    renderPopup: function () {
      var popup = document.getElementById('timer-popup');
      if (!popup) return;
      var self = this;
      var s = this.state;
      var body = document.getElementById('timer-popup-body');
      var modes = popup.querySelector('.timer-modes');
      var tab = document.getElementById('timer-mode-tab');
      var settingsLink = document.getElementById('timer-settings-link');
      var titleEl = document.getElementById('timer-popup-title');
      var labelEl = document.getElementById('timer-popup-label');
      // KF-008: the panel header names the mode; stopwatch counts "Session time".
      var headMode = s && s.mode ? s.mode : this.currentModeTab;
      var headLabel = headMode === 'pomodoro' ? 'Pomodoro' :
                      headMode === 'stopwatch' ? 'Stopwatch' : 'Break';
      if (titleEl) titleEl.textContent = headLabel;
      if (labelEl) labelEl.textContent =
        headMode === 'stopwatch' ? 'Session time' : 'Time until break';
      // KF-006: a session that ran to zero offers the break flow — "00:00"
      // with a single green Take break button next to the clock.
      if ((!s || s.phase === 'idle') && this.finished) {
        if (modes) modes.hidden = true;
        if (tab) tab.innerHTML = '';
        if (settingsLink) settingsLink.hidden = true;
        var isPom = this.finished.mode === 'pomodoro';
        if (body) body.innerHTML =
          '<div class="timer-session">' +
            '<div class="timer-finished-row">' +
              '<div class="timer-session-time">00:00</div>' +
              (isPom
                ? '<button type="button" class="btn btn-success" id="tp-take-break">Take break</button>'
                : '<button type="button" class="btn btn-success" id="tp-back-work">Back to work</button>') +
            '</div>' +
            '<button type="button" class="btn btn-link" id="tp-skip-finished">Skip</button>' +
          '</div>';
        var takeBtn = document.getElementById('tp-take-break');
        if (takeBtn) takeBtn.addEventListener('click', function () { self.takeBreak(); });
        var backBtn = document.getElementById('tp-back-work');
        if (backBtn) backBtn.addEventListener('click', function () { self.backToWork(); });
        var skipBtn = document.getElementById('tp-skip-finished');
        if (skipBtn) skipBtn.addEventListener('click', function () {
          self.finished = null;
          self.renderPopup();
        });
        this.renderTodayList();
        return;
      }
      if (!s || s.phase === 'idle') {
        if (body) body.innerHTML = '';
        if (modes) modes.hidden = false;
        this.setModeTab(this.currentModeTab);
        if (settingsLink) settingsLink.hidden = false;
        this.renderTodayList();
        return;
      }
      if (settingsLink) settingsLink.hidden = true;
      if (modes) modes.hidden = true;
      if (tab) tab.innerHTML = '';
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
            // No pause/resume: the server has no such endpoints; a running
            // session offers a red Stop control (KanbanFlow).
            '<button type="button" class="btn btn-danger" id="tp-stop">Stop</button>' +
            '<button type="button" class="btn btn-link" id="tp-switch">Switch task</button>' +
          '</div>' +
        '</div>';
      var stopBtn = document.getElementById('tp-stop');
      if (stopBtn) stopBtn.addEventListener('click', this.stopClicked.bind(this));
      var switchBtn = document.getElementById('tp-switch');
      if (switchBtn) switchBtn.addEventListener('click', this.changeTask.bind(this));
      this.startTick();
      this.renderTodayList();
    },

    // KF-138: populate the popup's Today list from GET /api/timer/today.
    renderTodayList: function () {
      var list = document.getElementById('timer-today-list');
      if (!list) return;
      fetch('/api/timer/today', {
        headers: { 'Accept': 'application/json' },
        credentials: 'same-origin',
      }).then(function (res) { return res.ok ? res.json() : null; })
        .then(function (entries) {
          if (!entries || !entries.length) {
            list.innerHTML = '<div class="timer-today-empty">No entries yet today.</div>';
            return;
          }
          list.innerHTML = entries.map(function (e) {
            var name = escapeHtml(e.task_name || e.kind_label || 'Time');
            var meta = escapeHtml((e.started_display || '') +
              (e.minutes != null ? ' · ' + e.minutes + 'm' : ''));
            var reason = e.interrupted && e.interrupt_reason
              ? ' <span class="today-reason">' + escapeHtml(e.interrupt_reason) + '</span>' : '';
            return '<div class="today-entry' + (e.interrupted ? ' stopped' : '') + '">' +
              '<span class="today-dot" style="background:' +
                (e.interrupted ? '#f87171' : '#4ade80') + '"></span>' +
              '<span class="today-task">' + name + reason + '</span>' +
              '<span class="today-meta">' + meta + '</span></div>';
          }).join('');
        })
        .catch(function () { /* today list is best-effort */ });
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
      self._tickCount = 0;
      self.tickHandle = window.setInterval(function () { self.tick(); }, 1000);
    },

    tick: function () {
      // KF-138: the server reports the mode string as phase ('pomodoro',
      // 'stopwatch', …) — 'running' never occurs, so the old check meant the
      // tick never fired. Any non-idle phase is live.
      var s = this.state;
      if (!s || s.phase === 'idle') return;
      // KF-079: ticking sound per the Ticking mode setting.
      var mode = this.settings && this.settings.ticking_mode;
      if (mode === 'always') {
        this.tickSound();
      } else if (mode === 'timer_start') {
        this._tickCount = (this._tickCount || 0) + 1;
        if (this._tickCount <= 5) this.tickSound();
      }
      if (s.mode === 'stopwatch') {
        // A stopwatch counts up and never completes on its own.
        this.renderPill();
        var upEl = document.querySelector('#timer-popup .timer-session-time');
        if (upEl && s.startedAt) {
          upEl.textContent = this.fmt(Math.max(0, Math.floor(Date.now() / 1000) - s.startedAt));
        }
        return;
      }
      s.remainingSeconds = (s.remainingSeconds || 0) - 1;
      if (s.remainingSeconds <= 0) {
        this.finishSession();
        return;
      }
      this.renderPill();
      var timeEl = document.querySelector('#timer-popup .timer-session-time');
      if (timeEl) timeEl.textContent = this.fmt(s.remainingSeconds);
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
      var s = this.settings;
      if (s && s.sounds_enabled === false) return;
      playAlarmSound(s && s.alarm_sound, s && s.alarm_volume);
    },

    // Ticking while a timer runs: "always" ticks every second,
    // "timer_start" ticks for the first 5 seconds, "never" is silent.
    tickSound: function () {
      var s = this.settings;
      if (!s || s.sounds_enabled === false) return;
      var mode = s.ticking_mode || 'never';
      if (mode === 'never') return;
      try {
        var Ctx = window.AudioContext || window.webkitAudioContext;
        if (!Ctx) return;
        var ctx = new Ctx();
        var o = ctx.createOscillator();
        var g = ctx.createGain();
        o.connect(g); g.connect(ctx.destination);
        o.type = 'square';
        o.frequency.value = 1000;
        var vol = Math.max(0, Math.min(100, s.points_volume != null ? s.points_volume : 70)) / 100;
        g.gain.setValueAtTime(0.001, ctx.currentTime);
        g.gain.exponentialRampToValueAtTime(Math.max(0.001, 0.12 * vol), ctx.currentTime + 0.01);
        g.gain.exponentialRampToValueAtTime(0.001, ctx.currentTime + 0.06);
        o.start(); o.stop(ctx.currentTime + 0.1);
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

    setModeTab: function (mode) {
      this.currentModeTab = mode;
      var self = this;
      document.querySelectorAll('#timer-popup .timer-modes [data-mode-tab]').forEach(function (b) {
        b.classList.toggle('active', b.getAttribute('data-mode-tab') === self.currentModeTab);
      });
      // KF-009: the footer first tab names the other mode.
      var foot = document.getElementById('timer-foot-mode');
      if (foot) {
        var other = mode === 'pomodoro' ? 'stopwatch' : 'pomodoro';
        var label = other === 'pomodoro' ? 'Pomodoro' : 'Stopwatch';
        foot.title = label;
        var span = foot.querySelector('span');
        if (span) span.textContent = label;
      }
      this.renderModeTab();
      // The idle pill reflects the selected tab (KF-098: stopwatch idle).
      this.renderPill();
    },

    switchModeTab: function () {
      this.setModeTab(this.currentModeTab === 'pomodoro' ? 'stopwatch' : 'pomodoro');
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
      // A new session supersedes any finished-session panel (KF-006).
      this.finished = null;
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

    stopClicked: function () {
      var s = this.state;
      if (!s || s.phase === 'idle') return;
      // KanbanFlow: sessions under 20s are discarded with a toast and no
      // "why did you stop?" menu (KF-007).
      var elapsed;
      if (s.mode === 'stopwatch' && s.startedAt) {
        elapsed = Math.max(0, Math.floor(Date.now() / 1000) - s.startedAt);
      } else {
        elapsed = Math.max(0, (s.totalSeconds || 0) - (s.remainingSeconds || 0));
      }
      if (elapsed < 20) {
        this.stopAndLog(null);
        return;
      }
      this.beginWhy(s.sessionId, s.taskId, s.taskName, s.mode, s.remainingSeconds, s.totalSeconds);
    },

    // KF-006: when a session runs to zero, log it as completed and offer
    // the break flow — the popup shows "00:00" with a green Take break
    // button (KanbanFlow). A long break starts every Nth completed
    // pomodoro (settings.long_break_every, default 4).
    finishSession: function () {
      var self = this;
      var finishedMode = self.state && self.state.mode ? self.state.mode : 'pomodoro';
      if (self.tickHandle) { window.clearInterval(self.tickHandle); self.tickHandle = null; }
      fetch('/api/timer/stop', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ completed: true, reason: 'completed' }),
      }).then(function () {
        self.playChime();
        self.finished = { mode: finishedMode, at: Date.now() };
        self.refresh();
        var popup = document.getElementById('timer-popup');
        if (popup && !popup.hidden) self.renderPopup();
      });
    },

    // KF-006: start the due break — long every Nth completed pomodoro.
    takeBreak: function () {
      var count = this.state && this.state.pomodoroCount ? this.state.pomodoroCount : 0;
      var every = this.settings && this.settings.long_break_every
        ? this.settings.long_break_every : 4;
      var kind = (count > 0 && count % every === 0) ? 'long' : 'short';
      this.finished = null;
      this.startBreak(kind);
    },

    // After a break runs to zero, offer the way back (inferred: KanbanFlow's
    // break-end panel is not in our evidence; a green Start Pomodoro is the
    // symmetric choice).
    backToWork: function () {
      this.finished = null;
      this.start('pomodoro', null);
    },

    // ----- "why did you stop?" flow (KF-005) -----

    beginWhy: function (sessionId, taskId, taskName, mode, remainingSeconds, totalSeconds) {
      var self = this;
      this.whyOriginal = this.state;
      this.whySessionId = sessionId;
      this.whyTaskId = taskId;
      this.whyTaskName = taskName || 'Pomodoro';
      this.whyMode = mode;
      this.whyEntryId = null;
      var menu = document.getElementById('why-stop-menu');
      // The menu lists the configured interruption reasons; make sure the
      // settings payload arrived before rendering it.
      this.ensureSettings(function () {
        self.renderWhyReasons();
        menu.hidden = false;
      });
      // Close the timer popup underneath; the why menu takes over.
      this.closePopup();
    },

    // Populate the menu from the configured interruption reasons (KF-005).
    // KanbanFlow's verbatim item order: the 15 defaults, "Add new reason…",
    // then "Task done" as the final item (KF-011). "Task done" seeded in
    // older databases is filtered out of the reason list since it is now
    // always the final menu item (KF-077).
    renderWhyReasons: function () {
      var wrap = document.getElementById('why-stop-reasons');
      if (!wrap) return;
      var self = this;
      var reasons = ((this.settings && this.settings.interrupt_reasons) || [])
        .filter(function (r) { return r.toLowerCase() !== 'task done'; });
      var html = '';
      reasons.forEach(function (r) {
        html += '<button type="button" data-why-reason="' + escapeHtml(r) + '">' +
          escapeHtml(r) + '</button>';
      });
      html += '<button type="button" class="why-add" id="why-add-new">Add new reason…</button>';
      html += '<button type="button" class="why-done" id="why-task-done">Task done</button>';
      wrap.innerHTML = html;
      var addRow = document.getElementById('why-stop-add-row');
      if (addRow) addRow.hidden = true;
      wrap.querySelectorAll('[data-why-reason]').forEach(function (btn) {
        btn.addEventListener('click', function () {
          self.stopAndLog(btn.getAttribute('data-why-reason'));
        });
      });
      var addNew = document.getElementById('why-add-new');
      if (addNew) addNew.addEventListener('click', function () {
        var row = document.getElementById('why-stop-add-row');
        if (row) row.hidden = false;
        var input = document.getElementById('why-stop-new');
        if (input) input.focus();
      });
      var done = document.getElementById('why-task-done');
      if (done) done.addEventListener('click', function () { self.whyTaskDone(); });
    },

    ensureSettings: function (cb) {
      var self = this;
      if (this.settings) { cb(); return; }
      fetch('/api/settings', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then(function (res) { return res.ok ? res.json() : null; })
        .then(function (s) { if (s) self.settings = s; cb(); });
    },

    closeWhyMenu: function () {
      var menu = document.getElementById('why-stop-menu');
      if (menu) menu.hidden = true;
    },

    stopAndLog: function (reason) {
      var self = this;
      var payload = {};
      if (reason) payload.reason = reason;
      fetch('/api/timer/stop', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify(payload),
      }).then(function (res) { return res.ok ? res.json() : null; })
        .then(function (data) {
          self.closeWhyMenu();
          self.refresh();
          // KF-007: the server discards sessions under 20s — say so with
          // KanbanFlow's toast instead of pretending the stop was logged.
          if (data && data.discarded) {
            toast('Session discarded<span class="toast-sub">Session lasted less than 20 seconds</span>');
          }
        });
    },

    // KF-021: the Add button must read the input instead of posting undefined.
    addWhyReason: function () {
      var input = document.getElementById('why-stop-new');
      var reason = input ? input.value.trim() : '';
      if (!reason) return;
      if (input) input.value = '';
      var addRow = document.getElementById('why-stop-add-row');
      if (addRow) addRow.hidden = true;
      this.stopAndLog(reason);
    },

    // KF-011: KanbanFlow's "Task done" is just a stop reason — it logs the
    // session with reason "Task done" and never moves the task anywhere.
    whyTaskDone: function () {
      this.stopAndLog('Task done');
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
    // KF-116: the route is GET /api/tasks/{id}/time and the modal renders
    // <div id="time-entries"> — both were wrong here before.
    var id = modalTaskId();
    if (id) fetchHtmlInto('/api/tasks/' + encodeURIComponent(id) + '/time', '#time-entries');
  }

  function positionCalendar(input, cal) {
    // The calendar popup is absolutely positioned inside the fixed overlay,
    // so client (viewport) coordinates are correct — no scroll offset.
    var r = input.getBoundingClientRect();
    cal.style.left = Math.min(r.left, window.innerWidth - 260) + 'px';
    cal.style.top = (r.bottom + 6) + 'px';
  }

  function isoDate(d) {
    return d.getFullYear() + '-' + String(d.getMonth() + 1).padStart(2, '0') + '-' +
      String(d.getDate()).padStart(2, '0');
  }

  function toTimeStr(d, withSeconds) {
    var s = String(d.getHours()).padStart(2, '0') + ':' +
      String(d.getMinutes()).padStart(2, '0');
    if (withSeconds) s += ':' + String(d.getSeconds()).padStart(2, '0');
    return s;
  }

  // Minutes between "HH:MM" (or "HH:MM:SS") strings; null when to <= from.
  function diffMinutes(from, to) {
    function parts(s) {
      var p = s.split(':');
      if (p.length < 2) return NaN;
      return (+p[0]) * 60 + (+p[1]) + (p[2] ? (+p[2]) / 60 : 0);
    }
    var a = parts(from), b = parts(to);
    if (isNaN(a) || isNaN(b) || b <= a) return null;
    return Math.floor(b - a);
  }

  function fmtDuration(mins) {
    var h = Math.floor(mins / 60), m = mins % 60;
    if (h > 0 && m > 0) return h + 'h ' + m + 'm';
    if (h > 0) return h + 'h';
    return m + 'm';
  }

  // Shared calendar renderer for the manual-time and edit-entry dialogs:
  // Sun–Sat grid, selected day blue (KF-003).
  function renderCalPopup(popup, input, year, month) {
    var first = new Date(year, month, 1);
    var startOffset = first.getDay(); // 0 = Sunday
    var daysInMonth = new Date(year, month + 1, 0).getDate();
    var monthName = first.toLocaleString('en-US', { month: 'long', year: 'numeric' });
    var html = '<div class="mt-cal-head"><button type="button" data-cal-prev>&lt;</button>' +
      '<span>' + monthName + '</span><button type="button" data-cal-next>&gt;</button></div>' +
      '<div class="mt-cal-grid">';
    ['S', 'M', 'T', 'W', 'T', 'F', 'S'].forEach(function (d) { html += '<span class="mt-cal-dow">' + d + '</span>'; });
    for (var i = 0; i < startOffset; i++) html += '<span></span>';
    for (var d = 1; d <= daysInMonth; d++) {
      var iso = year + '-' + String(month + 1).padStart(2, '0') + '-' + String(d).padStart(2, '0');
      var cls = 'mt-cal-day' + (iso === input.value ? ' selected' : '');
      html += '<button type="button" class="' + cls + '" data-date="' + iso + '">' + d + '</button>';
    }
    html += '</div>';
    popup.innerHTML = html;
    popup.querySelector('[data-cal-prev]').addEventListener('click', function (e) {
      e.stopPropagation();
      renderCalPopup(popup, input, month === 0 ? year - 1 : year, month === 0 ? 11 : month - 1);
    });
    popup.querySelector('[data-cal-next]').addEventListener('click', function (e) {
      e.stopPropagation();
      renderCalPopup(popup, input, month === 11 ? year + 1 : year, month === 11 ? 0 : month + 1);
    });
    popup.querySelectorAll('.mt-cal-day').forEach(function (btn) {
      btn.addEventListener('click', function (e) {
        e.stopPropagation();
        input.value = btn.getAttribute('data-date');
        popup.hidden = true;
      });
    });
  }

  // ---------- Manual time dialog (KF-003) ----------

  var ManualTime = {
    taskId: null,
    taskName: null,
    taskNameById: {},
    // date: optional YYYY-MM-DD to prefill the date field (KF-113 — the
    // timer log's per-day-group "Add time entry" links pass their day).
    open: function (taskId, taskName, date) {
      this.taskId = taskId || null;
      this.taskName = taskName || null;
      document.getElementById('manual-time-overlay').hidden = false;
      var taskInput = document.getElementById('mt-task');
      taskInput.value = taskName || '';
      document.getElementById('mt-date').value = date || isoDate(new Date());
      document.getElementById('mt-from').value = '';
      document.getElementById('mt-to').value = '';
      var comment = document.getElementById('mt-comment');
      comment.value = '';
      comment.hidden = true;
      document.getElementById('mt-comment-toggle').textContent = '+ Add comment';
      this.hideError();
      this.updateDuration();
      // Refresh the task list on every open so tasks created since the last
      // open resolve; submit() waits for the in-flight fetch.
      this.tasksPromise = this.loadTaskList();
      taskInput.focus();
    },
    close: function () {
      document.getElementById('manual-time-overlay').hidden = true;
      document.getElementById('mt-cal-popup').hidden = true;
    },
    closeError: function () {
      // The future-time overlay sits on top of the dialog; OK dismisses it
      // and leaves the dialog state intact.
      document.getElementById('mt-error-overlay').hidden = true;
    },
    // Task name list for the autocomplete datalist (shared with EditEntry).
    // Always refetches — tasks may have been created since the last open.
    loadTaskList: function () {
      var self = this;
      return fetch('/api/tasks', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then(function (res) { return res.ok ? res.json() : []; })
        .then(function (tasks) {
          var list = document.getElementById('mt-task-list');
          if (!list) return;
          var html = '';
          (tasks || []).forEach(function (t) {
            self.taskNameById[t.name] = t.id;
            html += '<option value="' + escapeHtml(t.name) + '"></option>';
          });
          list.innerHTML = html;
        })
        .catch(function () { /* datalist stays as-is on failure */ });
    },
    resolveTaskId: function () {
      var name = document.getElementById('mt-task').value.trim();
      if (name && (name in this.taskNameById)) return this.taskNameById[name];
      if (this.taskId && (!name || name === this.taskName)) return this.taskId;
      return null;
    },
    updateDuration: function () {
      var from = document.getElementById('mt-from').value;
      var to = document.getElementById('mt-to').value;
      var el = document.getElementById('mt-duration');
      if (!from || !to) { el.textContent = '0h'; return; }
      var mins = diffMinutes(from, to);
      el.textContent = mins === null ? '—' : fmtDuration(mins);
    },
    openCalendar: function () {
      var input = document.getElementById('mt-date');
      var popup = document.getElementById('mt-cal-popup');
      if (!popup.hidden) { popup.hidden = true; return; }
      var current = input.value ? new Date(input.value + 'T12:00:00') : new Date();
      renderCalPopup(popup, input, current.getFullYear(), current.getMonth());
      positionCalendar(input, popup);
      popup.hidden = false;
    },
    toggleComment: function () {
      var ta = document.getElementById('mt-comment');
      ta.hidden = !ta.hidden;
      document.getElementById('mt-comment-toggle').textContent =
        ta.hidden ? '+ Add comment' : 'Hide comment';
      if (!ta.hidden) ta.focus();
    },
    showError: function (message) {
      var err = document.getElementById('mt-error');
      if (err) { err.textContent = message; err.hidden = false; }
      else toast(message);
    },
    hideError: function () {
      var err = document.getElementById('mt-error');
      if (err) err.hidden = true;
    },
    submit: function () {
      var self = this;
      // Wait for the in-flight task-list refresh so a just-created task
      // resolves to its id.
      Promise.resolve(this.tasksPromise).then(function () { self.doSubmit(); });
    },
    doSubmit: function () {
      var self = this;
      var taskId = this.resolveTaskId();
      var date = document.getElementById('mt-date').value;
      var from = document.getElementById('mt-from').value;
      var to = document.getElementById('mt-to').value;
      var note = document.getElementById('mt-comment').value.trim();
      if (!taskId) { this.showError('Select a task from the list.'); return; }
      if (!date) { this.showError('Pick a date.'); return; }
      if (!from || !to) { this.showError('Enter a From and To time.'); return; }
      if (diffMinutes(from, to) === null) { this.showError('End time must be after start time.'); return; }
      fetch('/api/time/manual', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ task_id: taskId, date: date, from: from, to: to, note: note || null }),
      }).then(function (res) {
        if (res.ok) {
          self.close();
          if (modalTaskId()) { modalDirty = true; refreshModalTimeLog(); }
          else window.location.reload();
        } else {
          res.text().then(function (t) {
            if (/future/i.test(t || '')) {
              // KanbanFlow: dedicated error dialog on top, state preserved.
              document.getElementById('mt-error-overlay').hidden = false;
            } else {
              self.showError(t || 'Could not save the time entry.');
            }
          });
        }
      }).catch(function () { self.showError('Could not save the time entry.'); });
    },
  };

  // ---------- Edit time entry dialog (KF-004) ----------
  var EditEntry = {
    entryId: null,
    taskId: null,
    taskName: null,
    open: function (entryId) {
      var self = this;
      this.entryId = entryId;
      fetch('/api/time/entries/' + encodeURIComponent(entryId),
            { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then(function (res) { return res.ok ? res.json() : null; })
        .then(function (data) {
          if (!data) { toast('Could not load the time entry.'); return; }
          self.taskId = data.task_id;
          self.taskName = data.task_name || '';
          var start = new Date(data.started_at);
          document.getElementById('ee-date').value = isoDate(start);
          document.getElementById('ee-from').value = toTimeStr(start, true);
          document.getElementById('ee-to').value =
            toTimeStr(new Date(start.getTime() + (data.minutes || 0) * 60000), true);
          document.getElementById('ee-task').value = data.task_name || '';
          var comment = document.getElementById('ee-comment');
          comment.value = data.note || '';
          comment.hidden = true;
          document.getElementById('ee-comment-toggle').textContent = '+ Add comment';
          self.hideError();
          self.updateDuration();
          ManualTime.tasksPromise = ManualTime.loadTaskList();
          document.getElementById('edit-entry-overlay').hidden = false;
        });
    },
    close: function () {
      document.getElementById('edit-entry-overlay').hidden = true;
      document.getElementById('ee-cal-popup').hidden = true;
    },
    resolveTaskId: function () {
      var name = document.getElementById('ee-task').value.trim();
      if (name && (name in ManualTime.taskNameById)) return ManualTime.taskNameById[name];
      if (this.taskId && (!name || name === this.taskName)) return this.taskId;
      return null;
    },
    updateDuration: function () {
      var from = document.getElementById('ee-from').value;
      var to = document.getElementById('ee-to').value;
      var el = document.getElementById('ee-duration');
      if (!from || !to) { el.textContent = '0h'; return; }
      var mins = diffMinutes(from, to);
      el.textContent = mins === null ? '—' : fmtDuration(mins);
    },
    openCalendar: function () {
      var input = document.getElementById('ee-date');
      var popup = document.getElementById('ee-cal-popup');
      if (!popup.hidden) { popup.hidden = true; return; }
      var current = input.value ? new Date(input.value + 'T12:00:00') : new Date();
      renderCalPopup(popup, input, current.getFullYear(), current.getMonth());
      positionCalendar(input, popup);
      popup.hidden = false;
    },
    toggleComment: function () {
      var ta = document.getElementById('ee-comment');
      ta.hidden = !ta.hidden;
      document.getElementById('ee-comment-toggle').textContent =
        ta.hidden ? '+ Add comment' : 'Hide comment';
      if (!ta.hidden) ta.focus();
    },
    showError: function (message) {
      var err = document.getElementById('ee-error');
      if (err) { err.textContent = message; err.hidden = false; }
      else toast(message);
    },
    hideError: function () {
      var err = document.getElementById('ee-error');
      if (err) err.hidden = true;
    },
    submit: function () {
      var self = this;
      // Same task-list freshness guarantee as ManualTime.submit.
      Promise.resolve(ManualTime.tasksPromise).then(function () { self.doSubmit(); });
    },
    doSubmit: function () {
      var self = this;
      var taskId = this.resolveTaskId();
      var date = document.getElementById('ee-date').value;
      var from = document.getElementById('ee-from').value;
      var to = document.getElementById('ee-to').value;
      var note = document.getElementById('ee-comment').value.trim();
      if (!taskId) { this.showError('Select a task from the list.'); return; }
      if (!date) { this.showError('Pick a date.'); return; }
      if (!from || !to) { this.showError('Enter a From and To time.'); return; }
      if (diffMinutes(from, to) === null) { this.showError('End time must be after start time.'); return; }
      var btn = document.getElementById('ee-update');
      if (btn) { btn.disabled = true; btn.textContent = 'Updating…'; }
      fetch('/api/time/entries/' + encodeURIComponent(this.entryId), {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ task_id: taskId, date: date, from: from, to: to, note: note || null }),
      }).then(function (res) {
        if (btn) { btn.disabled = false; btn.textContent = 'Update'; }
        if (res.ok) {
          self.close();
          if (modalTaskId()) { modalDirty = true; refreshModalTimeLog(); }
          else window.location.reload();
        } else {
          res.text().then(function (t) {
            if (/future/i.test(t || '')) {
              document.getElementById('mt-error-overlay').hidden = false;
            } else {
              self.showError(t || 'Could not save the entry.');
            }
          });
        }
      }).catch(function () {
        if (btn) { btn.disabled = false; btn.textContent = 'Update'; }
        self.showError('Could not save the entry.');
      });
    },
  };

  // Guard: app.js is loaded with `defer`, so at execution time readyState is
  // already 'interactive' — both the immediate init call below AND the
  // DOMContentLoaded listener fire. Without this guard every handler here
  // (notably the dialog submits) would be bound twice.
  var entryEditInitialized = false;
  function initEntryEdit() {
    if (entryEditInitialized) return;
    entryEditInitialized = true;
    // The modal log renders data-entry-id (KF-004); accept the legacy
    // data-edit-entry attribute too.
    document.addEventListener('click', function (e) {
      var btn = e.target.closest('[data-edit-entry], [data-entry-id]');
      if (!btn) return;
      e.preventDefault();
      EditEntry.open(btn.getAttribute('data-edit-entry') || btn.getAttribute('data-entry-id'));
    });

    // Manual-time dialog wiring (mt-* ids, KF-003).
    var mtDate = document.getElementById('mt-date');
    if (mtDate) mtDate.addEventListener('click', function () { ManualTime.openCalendar(); });
    var mtCalBtn = document.getElementById('mt-cal-btn');
    if (mtCalBtn) mtCalBtn.addEventListener('click', function (e) { e.stopPropagation(); ManualTime.openCalendar(); });
    var mtFrom = document.getElementById('mt-from');
    if (mtFrom) mtFrom.addEventListener('input', function () { ManualTime.updateDuration(); });
    var mtTo = document.getElementById('mt-to');
    if (mtTo) mtTo.addEventListener('input', function () { ManualTime.updateDuration(); });
    var mtCommentBtn = document.getElementById('mt-comment-toggle');
    if (mtCommentBtn) mtCommentBtn.addEventListener('click', function () { ManualTime.toggleComment(); });
    var mtAdd = document.getElementById('mt-add');
    if (mtAdd) mtAdd.addEventListener('click', function () { ManualTime.submit(); });

    // Edit-entry dialog wiring (ee-* ids, KF-004).
    var eeDate = document.getElementById('ee-date');
    if (eeDate) eeDate.addEventListener('click', function () { EditEntry.openCalendar(); });
    var eeCalBtn = document.getElementById('ee-cal-btn');
    if (eeCalBtn) eeCalBtn.addEventListener('click', function (e) { e.stopPropagation(); EditEntry.openCalendar(); });
    var eeFrom = document.getElementById('ee-from');
    if (eeFrom) eeFrom.addEventListener('input', function () { EditEntry.updateDuration(); });
    var eeTo = document.getElementById('ee-to');
    if (eeTo) eeTo.addEventListener('input', function () { EditEntry.updateDuration(); });
    var eeCommentBtn = document.getElementById('ee-comment-toggle');
    if (eeCommentBtn) eeCommentBtn.addEventListener('click', function () { EditEntry.toggleComment(); });
    var eeUpdate = document.getElementById('ee-update');
    if (eeUpdate) eeUpdate.addEventListener('click', function () { EditEntry.submit(); });

    // "Add time" buttons elsewhere (timer log page day groups, KF-113/KF-119).
    // Document-level delegation so buttons rendered after boot are covered.
    document.addEventListener('click', function (e) {
      var btn = e.target.closest('[data-open-manual-time]');
      if (!btn) return;
      ManualTime.open(btn.getAttribute('data-task-id') || null,
                      btn.getAttribute('data-task-name') || null,
                      btn.getAttribute('data-date') || null);
    });
  }

  // ---------- keyboard shortcuts (KF-094, KF-095) ----------
  // KanbanFlow: T = timer menu, P = reports menu, Y = manual time entry,
  // E = time estimate. Task-modal context: V = Move dialog, . = More menu,
  // Cmd+Enter = save, Delete = delete task, Esc = close.

  function openReportsMenu() {
    var menu = document.getElementById('reports-menu');
    if (!menu) return;
    if (!menu.hidden) { menu.hidden = true; return; }
    // Interim anchor: the board-bar Menu + 15-item Reports submenu (KF-099,
    // KF-133, KF-136) do not exist yet — when they land, P should open the
    // real Reports submenu instead of this placeholder.
    var pill = document.getElementById('timer-pill');
    var x = window.innerWidth - 260;
    var y = 60;
    if (pill) {
      var r = pill.getBoundingClientRect();
      x = r.left;
      y = r.bottom + 8;
    }
    placeMenu(menu, x, y);
  }

  document.addEventListener('keydown', function (e) {
    var modal = document.querySelector('.task-modal[data-task-id]');
    var inField = !!(e.target && e.target.closest &&
      e.target.closest('input, textarea, select, [contenteditable]'));
    // Cmd/Ctrl+Enter: save changes (the modal persists name/description on
    // change; Enter here just commits the field and closes).
    if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key === 'Enter') {
      if (modal) {
        e.preventDefault();
        if (inField && e.target.blur) e.target.blur();
        closeModal();
      }
      return;
    }
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    // Escape must work even with focus inside a field (dialog inputs etc.).
    if (e.key === 'Escape') { handleEscape(); return; }
    if (inField) return;
    var key = e.key.toLowerCase();
    if (key === 't') {
      TimerUI.togglePopup();
    } else if (key === 'p') {
      openReportsMenu();
    } else if (key === 'y') {
      if (document.getElementById('manual-time-overlay')) {
        var yId = modalTaskId();
        var yNameInput = document.getElementById('modal-name');
        ManualTime.open(yId, yNameInput ? yNameInput.value : null);
      }
    } else if (key === 'e') {
      if (modal) {
        document.getElementById('est-input').value = '';
        document.getElementById('estimate-dialog').hidden = false;
        document.getElementById('est-input').focus();
      }
    } else if (key === 'v') {
      if (modal) openMoveDialog();
    } else if (key === '.') {
      if (modal) {
        var moreBtn = document.querySelector('[data-tm-menu="tm-more-menu"]');
        if (moreBtn) moreBtn.click();
      }
    } else if (key === 'delete') {
      if (modal) deleteModalTask();
    } else if (key === '?') {
      var shortcuts = document.getElementById('shortcuts-dialog');
      if (shortcuts) shortcuts.hidden = false;
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

  // ---------- timer log page (KF-090) ----------

  // KanbanFlow period presets -> [fromISO, toISO]. Weeks start Monday.
  function periodToRange(preset, customFrom, customTo, relN, relUnit) {
    var now = new Date();
    var today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
    function monday(d) {
      var x = new Date(d);
      var off = (x.getDay() + 6) % 7; // days since Monday
      x.setDate(x.getDate() - off);
      return x;
    }
    var from, to;
    switch (preset) {
      case 'this-week':
        from = monday(today);
        to = new Date(from); to.setDate(to.getDate() + 6);
        break;
      case 'last-week':
        from = monday(today); from.setDate(from.getDate() - 7);
        to = monday(today); to.setDate(to.getDate() - 1);
        break;
      case 'this-plus-last-week':
        from = monday(today); from.setDate(from.getDate() - 7);
        to = monday(today); to.setDate(to.getDate() + 6);
        break;
      case 'this-month':
        from = new Date(today.getFullYear(), today.getMonth(), 1);
        to = new Date(today.getFullYear(), today.getMonth() + 1, 0);
        break;
      case 'last-month':
        from = new Date(today.getFullYear(), today.getMonth() - 1, 1);
        to = new Date(today.getFullYear(), today.getMonth(), 0);
        break;
      case 'custom-absolute':
        from = customFrom ? new Date(customFrom + 'T12:00:00') : today;
        to = customTo ? new Date(customTo + 'T12:00:00') : today;
        break;
      case 'custom-relative': {
        var days = (relN || 14) * (relUnit === 'weeks' ? 7 : 1);
        to = today; from = new Date(today); from.setDate(from.getDate() - (days - 1));
        break;
      }
      case 'last-7':
        to = today; from = new Date(today); from.setDate(from.getDate() - 6);
        break;
      case 'last-14':
        to = today; from = new Date(today); from.setDate(from.getDate() - 13);
        break;
      case 'last-30':
        to = today; from = new Date(today); from.setDate(from.getDate() - 29);
        break;
      default:
        from = monday(today); from.setDate(from.getDate() - 7);
        to = monday(today); to.setDate(to.getDate() + 6);
    }
    if (from > to) { var t = from; from = to; to = t; }
    return [isoDay(from), isoDay(to)];
  }

  function isoDay(d) {
    return d.getFullYear() + '-' +
      String(d.getMonth() + 1).padStart(2, '0') + '-' +
      String(d.getDate()).padStart(2, '0');
  }

  function fetchJson(url) {
    return fetch(url, { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
      .then(function (res) {
        if (!res.ok) throw new Error('HTTP ' + res.status);
        return res.json();
      });
  }

  function downloadCsv(filename, rows) {
    var csv = rows.map(function (r) {
      return r.map(function (c) {
        return '"' + String(c == null ? '' : c).replace(/"/g, '""') + '"';
      }).join(',');
    }).join('\r\n');
    var blob = new Blob(['﻿' + csv], { type: 'text/csv;charset=utf-8' });
    var a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    setTimeout(function () { URL.revokeObjectURL(a.href); a.remove(); }, 100);
  }

  var TimerLogPage = (function () {
    var initialized = false;

    function qs(id) { return document.getElementById(id); }

    function currentFilters() {
      var period = qs('log-period-filter').value;
      var relN = parseInt(qs('log-relative-n').value, 10) || 14;
      var range = periodToRange(
        period,
        qs('log-custom-from').value, qs('log-custom-to').value,
        relN, qs('log-relative-unit').value);
      return {
        board_id: qs('log-board-filter').value,
        entry_type: qs('log-type-filter').value,
        from: range[0],
        to: range[1],
      };
    }

    function apiQuery(f) {
      var p = new URLSearchParams();
      if (f.board_id) p.set('board_id', f.board_id);
      if (f.entry_type) p.set('entry_type', f.entry_type);
      p.set('from', f.from);
      p.set('to', f.to);
      p.set('limit', '200');
      return p.toString();
    }

    // The log API paginates; day grouping must see the whole range, so
    // follow has_more until every page is in.
    function fetchAll(f) {
      var all = [];
      function page(offset) {
        return fetchJson('/api/timer/log?' + apiQuery(f) + '&offset=' + offset)
          .then(function (data) {
            all = all.concat(data.entries || []);
            if (data.has_more) return page(offset + 200);
            return all;
          });
      }
      return page(0);
    }

    function dayHeader(dateISO) {
      if (!dateISO) return 'Unknown date';
      var d = new Date(dateISO + 'T12:00:00');
      return d.toLocaleDateString('en-US', { weekday: 'long' }) + ', ' +
        d.toLocaleDateString('en-US', { day: 'numeric', month: 'long' });
    }

    function entryStatus(e) {
      if (e.kind === 'pomodoro') {
        return e.interrupted
          ? '<div class="log-stopped">Stopped Pomodoro with reason \u2018' +
            escapeHtml(e.interrupt_reason || 'No reason') + '\u2019</div>'
          : '<div class="log-success">Successful Pomodoro</div>';
      }
      if (e.kind === 'stopwatch') {
        return '<div class="log-neutral">Stopwatch session' +
          (e.interrupted && e.interrupt_reason ? ' — ' + escapeHtml(e.interrupt_reason) : '') +
          '</div>';
      }
      return '<div class="log-neutral">Manual entry</div>';
    }

    function renderLog(entries) {
      var list = qs('timer-log-list');
      if (!entries.length) {
        list.innerHTML = '<p class="log-status">No entries exist for the given filter</p>';
        return;
      }
      // Group by day; entries arrive newest-first so groups stay ordered.
      var order = [];
      var byKey = {};
      entries.forEach(function (e) {
        var key = e.day_key || '';
        if (!byKey[key]) { byKey[key] = []; order.push(key); }
        byKey[key].push(e);
      });
      var html = '';
      order.forEach(function (key) {
        var rows = byKey[key];
        var minutes = rows.reduce(function (s, e) { return s + (e.minutes || 0); }, 0);
        var pomos = rows.filter(function (e) { return e.kind === 'pomodoro'; }).length;
        var pomoWord = pomos === 1 ? 'Pomodoro' : 'Pomodoros';
        html += '<div class="log-day-group">' +
          '<div class="log-day-head"><span>' + escapeHtml(dayHeader(key)) +
          ' — ' + escapeHtml(fmtDuration(minutes)) + ' — ' + pomos + ' ' + pomoWord + '</span>' +
          '<button type="button" class="log-add-entry" data-open-manual-time' +
          ' data-date="' + escapeHtml(key) + '">Add time entry</button></div>';
        rows.forEach(function (e) {
          var dot = e.interrupted ? 'dot-orange' : 'dot-green';
          html += '<div class="log-entry">' +
            '<span class="timer-dot ' + dot + '"></span>' +
            '<span class="log-badge" title="' + escapeHtml(e.badge_title) + '">' +
            escapeHtml(e.badge_code) + '</span>' +
            '<div class="log-main"><div class="log-task">' + escapeHtml(e.task_name) + '</div>' +
            '<div class="log-when">' + escapeHtml(e.time_range) + '</div>' +
            entryStatus(e) +
            (e.note ? '<div class="log-note">' + escapeHtml(e.note) + '</div>' : '') +
            '</div><div class="log-dur">' + escapeHtml(fmtDuration(e.minutes)) + '</div></div>';
        });
        html += '</div>';
      });
      list.innerHTML = html;
    }

    function loadLog() {
      var list = qs('timer-log-list');
      list.innerHTML = '<p class="log-status">Loading…</p>';
      fetchAll(currentFilters()).then(function (entries) {
        renderLog(entries);
      }).catch(function () {
        list.innerHTML = '<p class="log-status">Could not load the timer log.</p>';
      });
    }

    function exportLogCsv() {
      fetchAll(currentFilters()).then(function (entries) {
        var rows = [['Date', 'Task', 'Board', 'Type', 'Duration', 'Time range', 'Status', 'Note']];
        entries.forEach(function (e) {
          var status = e.kind === 'pomodoro'
            ? (e.interrupted
              ? "Stopped Pomodoro with reason '" + (e.interrupt_reason || '') + "'"
              : 'Successful Pomodoro')
            : (e.kind === 'stopwatch' ? 'Stopwatch session' : 'Manual entry');
          rows.push([
            e.day_key, e.task_name, e.board_name || '', e.kind,
            fmtDuration(e.minutes), e.time_range, status, e.note || '',
          ]);
        });
        downloadCsv('timer-log.csv', rows);
      }).catch(function () { toast('Export failed.'); });
    }

    // ---------- Time spent report (KF-091) ----------
    var TimeSpent = {
      view: 'summary',
      setView: function (v) {
        this.view = v;
        document.getElementById('spent-view-summary').classList.toggle('active', v === 'summary');
        document.getElementById('spent-view-detailed').classList.toggle('active', v === 'detailed');
        loadSpent();
      },
      periodRange: function () {
        var preset = document.getElementById('spent-period').value;
        var map = {
          'last-7': 'last-7', 'last-14': 'last-14', 'last-30': 'last-30',
          'this-week': 'this-week', 'last-week': 'last-week',
          'this-month': 'this-month', 'last-month': 'last-month'
        };
        return periodToRange(map[preset] || 'last-30');
      }
    };
    window.TimeSpent = TimeSpent;

    function fmtDayLabel(dateStr) {
      // dateStr YYYY-MM-DD -> "Friday, 26 December 2025"
      var d = new Date(dateStr + 'T12:00:00');
      var days = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];
      var months = ['January', 'February', 'March', 'April', 'May', 'June',
        'July', 'August', 'September', 'October', 'November', 'December'];
      return days[d.getDay()] + ', ' + d.getDate() + ' ' + months[d.getMonth()] + ' ' + d.getFullYear();
    }

    function loadSpent() {
      var list = qs('spent-list');
      if (!list) return;
      var range = TimeSpent.periodRange();
      var p = new URLSearchParams({ from: range[0], to: range[1] });
      var color = qs('spent-color').value;
      if (color) p.set('color_id', color);
      list.innerHTML = '<p class="log-status">Loading&hellip;</p>';
      fetchJson('/api/timer/time-spent?' + p.toString()).then(function (rep) {
        qs('spent-total').textContent = 'Total: ' + fmtDuration(rep.total_minutes || 0);
        var days = (rep.days || []).filter(function (d) { return d.minutes > 0; });
        var group = qs('spent-group').value;
        days.sort(function (a, b) {
          return group === 'date-asc' ? (a.date < b.date ? -1 : 1) : (a.date > b.date ? -1 : 1);
        });
        if (!days.length) {
          list.innerHTML = '<p class="log-status">No entries exist for the given filter.</p>';
          return;
        }
        var html = '';
        days.forEach(function (d) {
          html += '<div class="spent-day"><div class="spent-day-head">' +
            '<span class="spent-day-label">' + escapeHtml(fmtDayLabel(d.date)) + '</span>' +
            '<span class="spent-day-total">' + escapeHtml(fmtDuration(d.minutes)) + '</span></div>';
          if (TimeSpent.view === 'detailed') {
            html += '<div class="spent-tasks">';
            (d.tasks || []).forEach(function (t) {
              html += '<div class="spent-task"><span class="spent-task-name">' +
                escapeHtml(t.task_name) + '</span>' +
                '<span class="spent-task-time">' + escapeHtml(fmtDuration(t.minutes)) + '</span></div>';
            });
            html += '</div>';
          }
          html += '</div>';
        });
        list.innerHTML = html;
      }).catch(function () {
        qs('spent-total').textContent = 'Could not load time spent.';
        list.innerHTML = '<p class="log-status">Could not load time spent.</p>';
      });
    }

    function loadSpentColors() {
      // Populate the Color filter from the first board's palette.
      var sel = qs('spent-color');
      if (!sel) return;
      fetchJson('/api/boards').then(function (boards) {
        if (!boards || !boards.length) return;
        return fetchJson('/api/boards/' + boards[0].id + '/colors');
      }).then(function (colors) {
        if (!colors) return;
        (colors.colors || colors).forEach(function (c) {
          var opt = document.createElement('option');
          opt.value = c.id;
          opt.textContent = c.label || c.name || c.id;
          sel.appendChild(opt);
        });
      }).catch(function () { /* color filter is best-effort */ });
    }

    function init() {
      if (initialized) return;
      initialized = true;
      if (!qs('timer-log-list')) return; // not the timer log page
      document.querySelectorAll('.timer-tab[data-tab]').forEach(function (tab) {
        tab.addEventListener('click', function () {
          document.querySelectorAll('.timer-tab[data-tab]').forEach(function (t) {
            t.classList.remove('active');
          });
          tab.classList.add('active');
          var isLog = tab.getAttribute('data-tab') === 'log';
          qs('tab-log').hidden = !isLog;
          qs('tab-spent').hidden = isLog;
          if (!isLog) loadSpent();
        });
      });
      qs('log-period-filter').addEventListener('change', function () {
        var v = this.value;
        qs('log-custom-absolute').hidden = v !== 'custom-absolute';
        qs('log-custom-relative').hidden = v !== 'custom-relative';
        if (v !== 'custom-absolute' && v !== 'custom-relative') loadLog();
      });
      ['log-custom-from', 'log-custom-to', 'log-relative-n', 'log-relative-unit',
       'log-board-filter', 'log-type-filter'].forEach(function (id) {
        qs(id).addEventListener('change', loadLog);
      });
      qs('log-reload').addEventListener('click', loadLog);
      qs('log-print').addEventListener('click', function () { window.print(); });
      var exportBtn = qs('log-export');
      var exportMenu = qs('log-export-menu');
      exportBtn.addEventListener('click', function (e) {
        e.stopPropagation();
        exportMenu.hidden = !exportMenu.hidden;
      });
      document.addEventListener('click', function (e) {
        if (!e.target.closest('.export-wrap')) exportMenu.hidden = true;
      });
      exportMenu.querySelector('[data-export="csv"]').addEventListener('click', function () {
        exportMenu.hidden = true;
        exportLogCsv();
      });
      qs('spent-filter-btn').addEventListener('click', function () {
        var pane = qs('spent-filter-pane');
        pane.hidden = !pane.hidden;
      });
      qs('spent-reload').addEventListener('click', loadSpent);
      qs('spent-period').addEventListener('change', loadSpent);
      qs('spent-color').addEventListener('change', loadSpent);
      qs('spent-group').addEventListener('change', loadSpent);
      qs('spent-print').addEventListener('click', function () { window.print(); });
      qs('spent-export').addEventListener('click', function () { toast('Export is coming soon.'); });
      qs('spent-label-clear').addEventListener('click', function () { qs('spent-label').value = ''; });
      loadSpentColors();
      loadLog();
    }

    return { init: init, reload: loadLog };
  })();

  // ---------- timer statistics page (KF-092) ----------

  var TimerStatsPage = (function () {
    var initialized = false;

    function qs(id) { return document.getElementById(id); }

    function currentRange() {
      var period = qs('stats-period-filter').value;
      var relN = parseInt(qs('stats-relative-n').value, 10) || 30;
      return periodToRange(
        period,
        qs('stats-custom-from').value, qs('stats-custom-to').value,
        relN, qs('stats-relative-unit').value);
    }

    function weekday(dateISO) {
      return new Date(dateISO + 'T12:00:00')
        .toLocaleDateString('en-US', { weekday: 'long' });
    }

    function statCard(value, label) {
      return '<div class="stat-card"><div class="stat-value">' + escapeHtml(String(value)) +
        '</div><div class="stat-label">' + escapeHtml(label) + '</div></div>';
    }

    function render(rep) {
      qs('stats-summary').innerHTML =
        statCard(rep.total_pomodori, 'Pomodori') +
        statCard(fmtDuration(rep.total_minutes), 'Total time') +
        statCard(fmtDuration(rep.avg_minutes), 'Average pomodoro') +
        statCard(rep.interruptions, 'Interruptions');

      // Pomodoros tab: daily bar chart with weekday tooltips.
      var chart = qs('stats-chart');
      if (!rep.total_pomodori) {
        chart.innerHTML = '<p class="log-status">No data to display</p>';
      } else {
        renderBarChart(chart, (rep.daily || []).map(function (d) {
          var unit = d.pomodori === 1 ? 'pomodoro' : 'pomodoros';
          return {
            label: d.label,
            minutes: d.pomodori,
            tooltip: weekday(d.date) + ', ' + d.label + ': ' + d.pomodori + ' ' + unit,
          };
        }));
      }

      // Interruptions tab: counts by reason.
      var reasons = rep.by_reason || [];
      var rh;
      if (!reasons.length) {
        rh = '<p class="log-status">No data to display</p>';
      } else {
        var max = 1;
        reasons.forEach(function (r) { if (r.count > max) max = r.count; });
        rh = '';
        reasons.forEach(function (r) {
          var pct = Math.round(100 * r.count / max);
          rh += '<div class="reason-row"><span class="reason-name">' + escapeHtml(r.reason) +
            '</span><div class="reason-bar"><div class="reason-fill reason-interrupted" ' +
            'style="width:' + pct + '%"></div></div>' +
            '<span class="reason-count">' + r.count + '</span></div>';
        });
      }
      qs('stats-reasons').innerHTML = rh;

      // Highscores tab.
      var hs = '';
      if (rep.best_day) {
        hs += statCard(rep.best_day.pomodori,
          'Best day — ' + weekday(rep.best_day.date) + ', ' + rep.best_day.label);
      }
      hs += statCard(rep.longest_streak, 'Longest streak (days)');
      qs('stats-highscores').innerHTML = hs || '<p class="log-status">No data to display</p>';
    }

    function load() {
      var range = currentRange();
      var p = new URLSearchParams({ from: range[0], to: range[1] });
      var board = qs('stats-board-filter').value;
      if (board) p.set('board_id', board);
      qs('stats-chart').innerHTML = '<p class="log-status">Loading chart…</p>';
      fetchJson('/api/timer/statistics?' + p.toString()).then(function (rep) {
        render(rep);
      }).catch(function () {
        qs('stats-chart').innerHTML = '<p class="log-status">Could not load statistics.</p>';
      });
    }

    function exportCsv() {
      var range = currentRange();
      var p = new URLSearchParams({ from: range[0], to: range[1] });
      var board = qs('stats-board-filter').value;
      if (board) p.set('board_id', board);
      fetchJson('/api/timer/statistics?' + p.toString()).then(function (rep) {
        var rows = [['Date', 'Pomodori']];
        (rep.daily || []).forEach(function (d) { rows.push([d.date, d.pomodori]); });
        downloadCsv('pomodoro-statistics.csv', rows);
      }).catch(function () { toast('Export failed.'); });
    }

    function init() {
      if (initialized) return;
      initialized = true;
      if (!qs('stats-chart')) return; // not the statistics page
      document.querySelectorAll('.stats-tab[data-tab]').forEach(function (tab) {
        tab.addEventListener('click', function () {
          document.querySelectorAll('.stats-tab[data-tab]').forEach(function (t) {
            t.classList.remove('active');
          });
          tab.classList.add('active');
          var current = tab.getAttribute('data-tab');
          ['pomodoros', 'interruptions', 'breaks', 'highscores'].forEach(function (t) {
            qs('stats-tab-' + t).hidden = t !== current;
          });
        });
      });
      qs('stats-period-filter').addEventListener('change', function () {
        var v = this.value;
        qs('stats-custom-absolute').hidden = v !== 'custom-absolute';
        qs('stats-custom-relative').hidden = v !== 'custom-relative';
        if (v !== 'custom-absolute' && v !== 'custom-relative') load();
      });
      ['stats-custom-from', 'stats-custom-to', 'stats-relative-n', 'stats-relative-unit',
       'stats-board-filter'].forEach(function (id) {
        qs(id).addEventListener('change', load);
      });
      qs('stats-reload').addEventListener('click', load);
      qs('stats-export').addEventListener('click', exportCsv);
      load();
    }

    return { init: init, reload: load };
  })();

  function renderBarChart(el, bars) {
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
      var tip = b.tooltip || (b.label + ': ' + b.minutes + ' min');
      html += '<rect x="' + x.toFixed(1) + '" y="' + y.toFixed(1) + '" width="' + bw.toFixed(1) +
        '" height="' + h.toFixed(1) + '" fill="#2f7cf6" rx="2">' +
        '<title>' + escapeHtml(tip) + '</title></rect>' +
        '<text x="' + (padL + slot * i + slot / 2).toFixed(1) + '" y="' + (H - 10) +
        '" text-anchor="middle" font-size="11" fill="#777">' + escapeHtml(b.label) + '</text>';
    });
    // Wrap in a real <svg> so the shapes parse in the SVG namespace
    // (setting them as a div's innerHTML renders nothing).
    el.innerHTML = '<svg class="bar-svg" viewBox="0 0 ' + W + ' ' + H +
      '" role="img">' + html + '</svg>';
  }

  // ---------- board chrome: board-bar controls, filter panel, board menu ----------
  // KF-132 (timer pill lives in the light board bar), KF-133 (board-bar
  // buttons), KF-134/KF-096 (filter panel), KF-135/KF-099 (board menu),
  // KF-136 (reports submenu). Guarded init like the other inits (KF-137).

  var BoardChrome = {
    initialized: false,
    closeBoardMenu: null,

    init: function () {
      if (this.initialized) return;
      this.initialized = true;
      if (!document.querySelector('.board-wrap')) return;
      this.restorePrefs();
      this.initMenu();
      this.initFilter();
      this.initBarButtons();
      this.initEsc();
    },

    // ----- preferences -----
    restorePrefs: function () {
      try {
        if (window.localStorage.getItem('chipflow-dark') === '1') document.body.classList.add('dark');
        if (window.localStorage.getItem('chipflow-large-names') === '1') document.body.classList.add('large-names');
      } catch (e) { /* storage unavailable */ }
    },
    setPref: function (key, on) {
      try {
        if (on) window.localStorage.setItem(key, '1');
        else window.localStorage.removeItem(key);
      } catch (e) { /* storage unavailable */ }
    },

    // ----- board menu + reports submenu -----
    initMenu: function () {
      var self = this;
      var btn = document.getElementById('board-menu-btn');
      var menu = document.getElementById('board-menu');
      var sub = document.getElementById('reports-submenu');
      if (!btn || !menu || !sub) return;

      function placeMenu() {
        var r = btn.getBoundingClientRect();
        menu.style.top = (r.bottom + 6) + 'px';
        menu.style.right = Math.max(8, window.innerWidth - r.right) + 'px';
        menu.style.left = 'auto';
      }
      function placeSub() {
        var mr = menu.getBoundingClientRect();
        sub.style.top = mr.top + 'px';
        // The menu sits at the right edge: fly the submenu out to its left.
        sub.style.right = (window.innerWidth - mr.left + 4) + 'px';
        sub.style.left = 'auto';
      }
      var reportsBtn = menu.querySelector('[data-bm="reports"]');
      function closeSub() {
        sub.hidden = true;
        if (reportsBtn) reportsBtn.setAttribute('aria-expanded', 'false');
      }
      function closeMenu() {
        menu.hidden = true;
        closeSub();
        btn.setAttribute('aria-expanded', 'false');
      }
      this.closeBoardMenu = closeMenu;

      btn.addEventListener('click', function (e) {
        e.stopPropagation();
        if (menu.hidden) {
          placeMenu();
          menu.hidden = false;
          btn.setAttribute('aria-expanded', 'true');
        } else {
          closeMenu();
        }
      });

      if (reportsBtn) {
        reportsBtn.addEventListener('click', function (e) {
          e.stopPropagation();
          if (sub.hidden) {
            placeSub();
            sub.hidden = false;
            reportsBtn.setAttribute('aria-expanded', 'true');
          } else {
            closeSub();
          }
        });
        reportsBtn.addEventListener('mouseenter', function () {
          if (!menu.hidden && sub.hidden) {
            placeSub();
            sub.hidden = false;
            reportsBtn.setAttribute('aria-expanded', 'true');
          }
        });
      }

      menu.addEventListener('click', function (e) {
        var item = e.target.closest('[data-bm]');
        if (!item || item === reportsBtn) return;
        e.stopPropagation();
        closeMenu();
        self.menuAction(item.getAttribute('data-bm'));
      });

      sub.addEventListener('click', function (e) {
        var item = e.target.closest('[data-report]');
        if (!item) return;
        e.stopPropagation();
        var key = item.getAttribute('data-report');
        var label = item.textContent.trim();
        closeMenu();
        self.reportAction(key, label);
      });

      document.addEventListener('click', function (e) {
        if (!menu.hidden &&
            !e.target.closest('#board-menu') &&
            !e.target.closest('#reports-submenu') &&
            !e.target.closest('#board-menu-btn')) {
          closeMenu();
        }
      });
    },

    menuAction: function (action) {
      switch (action) {
        case 'filter':
          this.openFilter();
          break;
        case 'layout':
          this.editLayout();
          break;
        case 'settings':
          window.location.href = '/settings';
          break;
        case 'members':
          this.peopleDialog('Members', this.ownerName() + ' \u2014 Board owner.');
          break;
        case 'recycle':
          toast('ChipFlow has no recycle bin \u2014 deleted tasks are removed permanently.');
          break;
        case 'dark':
          this.toggleDark();
          break;
        case 'legend':
          this.showLegend();
          break;
        case 'large-names':
          this.toggleLargeNames();
          break;
        case 'help':
          this.openHelp();
          break;
        case 'premium':
          this.peopleDialog('Get Premium', 'ChipFlow is free and open-source \u2014 every feature is already unlocked.');
          break;
      }
    },

    reportAction: function (key, label) {
      switch (key) {
        case 'stats':
          window.location.href = '/timer/statistics';
          break;
        case 'time':
          window.location.href = '/timer/log';
          break;
        case 'print':
          window.print();
          break;
        default:
          toast(label + ' report is not available in ChipFlow yet.');
          break;
      }
    },

    // ----- filter panel -----
    initFilter: function () {
      var self = this;
      var btn = document.getElementById('filter-btn');
      var panel = document.getElementById('filter-panel');
      if (!btn || !panel) return;
      this.addDateOptions();
      btn.addEventListener('click', function (e) {
        e.stopPropagation();
        self.toggleFilter();
      });
      var close = document.getElementById('filter-close');
      if (close) close.addEventListener('click', function () { self.closeFilter(); });
      panel.addEventListener('change', function (e) {
        if (e.target.name === 'f-user' || e.target.name === 'f-color' || e.target.name === 'f-date') {
          self.applyFilter();
          self.maybeSaveFilter();
        } else if (e.target.id === 'filter-remember') {
          self.maybeSaveFilter();
        }
      });
      this.restoreFilter();
    },

    toggleFilter: function () {
      var panel = document.getElementById('filter-panel');
      if (!panel) return;
      panel.hidden = !panel.hidden;
      this.syncFilterBtn();
    },
    openFilter: function () {
      var panel = document.getElementById('filter-panel');
      if (!panel) return;
      panel.hidden = false;
      this.syncFilterBtn();
    },
    closeFilter: function () {
      var panel = document.getElementById('filter-panel');
      if (!panel) return;
      panel.hidden = true;
      this.syncFilterBtn();
    },
    syncFilterBtn: function () {
      var btn = document.getElementById('filter-btn');
      var panel = document.getElementById('filter-panel');
      if (!btn || !panel) return;
      btn.classList.toggle('active', !panel.hidden || this.isFiltering());
    },
    isFiltering: function () {
      return this.filterValue('f-user') !== 'all' ||
        this.filterValue('f-color') !== 'all' ||
        this.filterValue('f-date') !== 'all';
    },
    filterValue: function (name) {
      var el = document.querySelector('input[name="' + name + '"]:checked');
      return el ? el.value : 'all';
    },
    setRadio: function (name, value) {
      if (!value) return;
      var el = document.querySelector('input[name="' + name + '"][value="' + value + '"]');
      if (el) el.checked = true;
    },

    applyFilter: function () {
      var user = this.filterValue('f-user');
      var color = this.filterValue('f-color');
      // Tasks carry no due dates or labels: those filter options exist for
      // parity but do not change the card set.
      var activeTaskId = null;
      if (user === 'timer' && window.TimerUI && TimerUI.state && TimerUI.state.taskId) {
        activeTaskId = String(TimerUI.state.taskId);
      }
      document.querySelectorAll('.task-card').forEach(function (card) {
        var show = true;
        if (color !== 'all' && card.dataset.colorValue !== color) show = false;
        // "Timer users": only the card with the running timer stays visible.
        // Unassigned / a named user: no assignee data exists — show all.
        if (user === 'timer' && String(card.dataset.taskId) !== activeTaskId) show = false;
        card.style.display = show ? '' : 'none';
      });
      this.syncFilterBtn();
    },

    maybeSaveFilter: function () {
      var remember = document.getElementById('filter-remember');
      try {
        if (remember && remember.checked) {
          window.localStorage.setItem('chipflow-filter', JSON.stringify({
            user: this.filterValue('f-user'),
            color: this.filterValue('f-color'),
            date: this.filterValue('f-date')
          }));
        } else {
          window.localStorage.removeItem('chipflow-filter');
        }
      } catch (e) { /* storage unavailable */ }
    },
    restoreFilter: function () {
      var raw = null;
      try { raw = window.localStorage.getItem('chipflow-filter'); } catch (e) { /* ignore */ }
      if (!raw) return;
      try {
        var f = JSON.parse(raw);
        this.setRadio('f-user', f.user);
        this.setRadio('f-color', f.color);
        this.setRadio('f-date', f.date);
        var remember = document.getElementById('filter-remember');
        if (remember) remember.checked = true;
        this.applyFilter();
      } catch (e) { /* corrupt saved filter */ }
    },

    // Month/year date options depend on the current date — render in JS.
    addDateOptions: function () {
      var wrap = document.getElementById('filter-date-options');
      if (!wrap || wrap.dataset.extended) return;
      wrap.dataset.extended = '1';
      var now = new Date();
      var months = ['January', 'February', 'March', 'April', 'May', 'June',
        'July', 'August', 'September', 'October', 'November', 'December'];
      var nm = new Date(now.getFullYear(), now.getMonth() + 1, 1);
      var specs = [
        ['month:' + now.getFullYear() + '-' + now.getMonth(), 'Due in ' + months[now.getMonth()]],
        ['month:' + nm.getFullYear() + '-' + nm.getMonth(), 'Due in ' + months[nm.getMonth()]],
        ['year:' + now.getFullYear(), 'Due in ' + now.getFullYear()],
        ['year:' + (now.getFullYear() + 1), 'Due in ' + (now.getFullYear() + 1)]
      ];
      specs.forEach(function (s) {
        var label = document.createElement('label');
        label.className = 'filter-opt';
        var input = document.createElement('input');
        input.type = 'radio';
        input.name = 'f-date';
        input.value = s[0];
        label.appendChild(input);
        label.appendChild(document.createTextNode(' ' + s[1]));
        wrap.appendChild(label);
      });
    },

    // ----- board-bar buttons -----
    initBarButtons: function () {
      var self = this;
      var invite = document.getElementById('invite-btn');
      if (invite) invite.addEventListener('click', function () {
        self.peopleDialog('Invite to board', 'ChipFlow is single-user \u2014 boards cannot be shared yet.');
      });
      var layout = document.getElementById('edit-layout-btn');
      if (layout) layout.addEventListener('click', function () { self.editLayout(); });
    },

    editLayout: function () {
      toast('Board layout editor is not available yet \u2014 use the column \u22EE menus to edit, move, add, or delete columns.');
    },

    peopleDialog: function (title, body) {
      var dlg = document.getElementById('people-dialog');
      if (!dlg) return;
      document.getElementById('people-dialog-title').textContent = title;
      document.getElementById('people-dialog-body').textContent = body;
      dlg.hidden = false;
    },

    ownerName: function () {
      var av = document.querySelector('.owner-avatar');
      if (av && av.title) {
        var m = av.title.match(/Board owner:\s*(.*)/);
        if (m) return m[1];
      }
      var u = document.querySelector('.topbar .user');
      return u ? u.textContent.trim() : 'you';
    },

    toggleDark: function () {
      var on = !document.body.classList.contains('dark');
      document.body.classList.toggle('dark', on);
      this.setPref('chipflow-dark', on);
      toast(on ? 'Dark mode on.' : 'Dark mode off.');
    },
    toggleLargeNames: function () {
      var on = !document.body.classList.contains('large-names');
      document.body.classList.toggle('large-names', on);
      this.setPref('chipflow-large-names', on);
      toast(on ? 'Large task names on.' : 'Large task names off.');
    },
    showLegend: function () {
      var legend = document.querySelector('.color-legend');
      if (!legend) {
        toast('No color legend on this board.');
        return;
      }
      legend.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
      legend.classList.add('legend-flash');
      window.setTimeout(function () { legend.classList.remove('legend-flash'); }, 1600);
    },
    openHelp: function () {
      var dlg = document.getElementById('shortcuts-dialog');
      if (dlg) dlg.hidden = false;
      else toast('Help is not available.');
    },

    // Escape for the filter panel. The unified Escape handler already
    // covers the board menu (hideFloatingMenus) and the people dialog
    // (.dlg-overlay); the panel is not a menu-pop, so it needs its own.
    initEsc: function () {
      var self = this;
      document.addEventListener('keydown', function (e) {
        if (e.key !== 'Escape') return;
        var panel = document.getElementById('filter-panel');
        if (panel && !panel.hidden) self.closeFilter();
      });
    }
  };

  // ---------- boot ----------

  // Guard: the defer/DOMContentLoaded double-fire (KF-137) would otherwise
  // bind every board handler twice.
  var boardInitialized = false;
  function initBoard() {
    if (boardInitialized) return;
    boardInitialized = true;
    initTaskSortable();
    initColumnSortable();
    applyCollapsedColumns();
    initAddTask();
    initBoardMenus();
    initMembersDialog();

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
    TimerLogPage.init();
    TimerStatsPage.init();
    TimerUI.init();
    BoardChrome.init();
  });
  // In case app.js runs after DOMContentLoaded (defer ordering):
  if (document.readyState !== 'loading') {
    if (document.querySelector('.board-wrap')) initBoard();
    initEntryEdit();
    TimerLogPage.init();
    TimerStatsPage.init();
    TimerUI.init();
    BoardChrome.init();
  }

  // Exposed for inline handlers and debugging.
  window.closeModal = closeModal;
  window.TimerUI = TimerUI;
  window.ManualTime = ManualTime;
  window.EditEntry = EditEntry;
  window.TimerLogPage = TimerLogPage;
  window.TimerStatsPage = TimerStatsPage;
  window.BoardChrome = BoardChrome;
})();
