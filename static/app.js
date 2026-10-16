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
  // KF-334: the Move task dialog's task and target, held between
  // openMoveDialog() and doMoveTask().
  var moveTargetColumnId = null;
  var moveSwimlaneId = null;
  var moveTaskId = null;

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
            // KF-199: the server-rendered card carries no client-side timer
            // state — re-apply the timer outline and live badge immediately
            // so a move while the timer runs re-renders at once instead of
            // waiting for the next 15s status poll.
            if (typeof TimerUI !== 'undefined') {
              if (TimerUI.updateCardIndicators) TimerUI.updateCardIndicators();
              if (TimerUI.updateCardLive) TimerUI.updateCardLive();
            }
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
    // KF-161: if a filter is active, the header shows the visible subset —
    // recompute it rather than stamping the true total.
    if (window.BoardChrome && BoardChrome.isFiltering && BoardChrome.isFiltering()) {
      BoardChrome.refreshFilteredCounts();
      return;
    }
    var wip = th.dataset.wipLimit ? parseInt(th.dataset.wipLimit, 10) : null;
    var el = th.querySelector('.columnHeader-count');
    if (el) el.textContent = wip ? count + ' / ' + wip : String(count);
    // KF-208: non-WIP columns show the count in the right-edge badge instead.
    var badge = th.querySelector('.columnHeader-countBadge');
    if (badge) {
      badge.textContent = String(count);
    }
    // KF-035: the warning is a *violation* — it fires only when the count
    // exceeds the limit, not when it merely reaches it.
    var warn = wip != null && count > wip;
    if (badge) {
      // KF-241: the collapsed strip's violated count must toggle the darkred
      // --over class along with the warning classes below.
      badge.classList.toggle('columnHeader-countBadge--over', warn);
    }
    th.classList.toggle('columnHeader--warning', warn);
    th.classList.toggle('columnHeader--limitWarning', warn);
    syncWarningLine(th, warn);
  }

  // KF-223: a collapsed strip also keeps the line for overdue tasks.
  function syncWarningLine(th, warn) {
    var show = warn || (th.dataset.collapsed === '1' && parseInt(th.dataset.overdueCount, 10) > 0);
    var line = th.querySelector('.columnHeader-warningLine');
    if (show && !line) {
      line = document.createElement('div');
      line.className = 'columnHeader-warningLine';
      line.setAttribute('aria-hidden', 'true');
      th.appendChild(line);
    } else if (!show && line) {
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
      // KF-048: the dragged column renders blank/empty mid-drag
      // (KanbanFlow parity); the placeholder marks the drop position.
      dragClass: 'column-drag-ghost',
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

  // ---------- layout-view arrange lists (KF-180) ----------

  // The layout-edit view lists the existing columns and swimlanes with
  // drag-reorder (SortableJS), persisted via the same move endpoints the
  // board's column drag and the ⋮ menus use.
  function initLayoutSortables() {
    if (typeof Sortable === 'undefined') return;
    var colList = document.getElementById('layout-columns-list');
    if (colList && !colList._sortable) {
      colList._sortable = new Sortable(colList, {
        animation: 150,
        handle: '.layout-list-handle',
        ghostClass: 'sortable-placeholder',
        onEnd: function (evt) {
          var items = Array.prototype.slice.call(colList.querySelectorAll('[data-column-id]'));
          var newIndex = items.findIndex(function (li) { return li.dataset.columnId === evt.item.dataset.columnId; });
          api('/api/columns/' + encodeURIComponent(evt.item.dataset.columnId) + '/move', 'POST', { position: newIndex })
            .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not reorder columns.'); })
            .catch(function () { toast('Could not reorder columns.'); });
        },
      });
    }
    var laneList = document.getElementById('layout-swimlanes-list');
    if (laneList && !laneList._sortable) {
      laneList._sortable = new Sortable(laneList, {
        animation: 150,
        handle: '.layout-list-handle',
        ghostClass: 'sortable-placeholder',
        onEnd: function (evt) {
          var items = Array.prototype.slice.call(laneList.querySelectorAll('[data-swimlane-id]'));
          var newIndex = items.findIndex(function (li) { return li.dataset.swimlaneId === evt.item.dataset.swimlaneId; });
          api('/api/swimlanes/' + encodeURIComponent(evt.item.dataset.swimlaneId) + '/move', 'POST', { position: newIndex })
            .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not reorder swimlanes.'); })
            .catch(function () { toast('Could not reorder swimlanes.'); });
        },
      });
    }
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

  // ---------- add-task popup ----------

  // KF-208: clamp a fixed-position popup so it always opens fully in view.
  function placePopup(popup, x, y) {
    popup.style.left = '0px';
    popup.style.top = '0px';
    var w = popup.offsetWidth;
    var h = popup.offsetHeight;
    popup.style.left = Math.max(4, Math.min(x, window.innerWidth - w - 4)) + 'px';
    popup.style.top = Math.max(4, Math.min(y, window.innerHeight - h - 4)) + 'px';
  }

  // KF-209: make a floating popup draggable by any non-interactive area
  // (KanbanFlow parity). Drag is clamped to the viewport.
  function makeDraggable(popup) {
    popup.addEventListener('mousedown', function (e) {
      if (e.button !== 0) return;
      if (e.target.closest('input, textarea, select, button, a, label, [data-no-drag]')) return;
      e.preventDefault();
      var startX = e.clientX, startY = e.clientY;
      var rect = popup.getBoundingClientRect();
      var origLeft = rect.left, origTop = rect.top;
      popup.style.left = origLeft + 'px';
      popup.style.top = origTop + 'px';
      popup.style.right = 'auto';
      popup.style.bottom = 'auto';
      function onMove(ev) {
        var w = popup.offsetWidth, h = popup.offsetHeight;
        var nx = Math.max(4, Math.min(origLeft + (ev.clientX - startX), window.innerWidth - w - 4));
        var ny = Math.max(4, Math.min(origTop + (ev.clientY - startY), window.innerHeight - h - 4));
        popup.style.left = nx + 'px';
        popup.style.top = ny + 'px';
      }
      function onUp() {
        document.removeEventListener('mousemove', onMove);
        document.removeEventListener('mouseup', onUp);
      }
      document.addEventListener('mousemove', onMove);
      document.addEventListener('mouseup', onUp);
    });
  }

  function initAddTask() {
    document.addEventListener('click', function (e) {
      var btn = e.target.closest('[data-add-task-for]');
      if (!btn) return;
      // Toggle: clicking the same column's "+" closes its popup.
      var colId = btn.getAttribute('data-add-task-for');
      var existing = document.querySelector('.add-task-popup');
      if (existing) {
        var wasSame = existing.dataset.columnId === colId;
        existing.remove();
        if (wasSame) return;
      }
      hideFloatingMenus();
      var tpl = document.getElementById('add-task-form-template');
      if (!tpl) return;
      var frag = tpl.content.cloneNode(true);
      var form = frag.querySelector('form');
      // KF-208: the popup floats outside the task-list (which may be hidden
      // inside a folded swimlane strip, putting an inline form out of view),
      // so strip the htmx wiring and submit via fetch — the server returns
      // the rendered card HTML fragment either way.
      form.removeAttribute('hx-post');
      form.removeAttribute('hx-target');
      form.removeAttribute('hx-swap');
      form.removeAttribute('hx-on::after-request');
      form.querySelector('input[name="column_id"]').value = colId;
      var firstList = document.querySelector('.task-list[data-column-id="' + cssEscape(colId) + '"]');
      form.querySelector('input[name="swimlane_id"]').value = firstList ? (firstList.dataset.swimlaneId || '') : '';
      var popup = document.createElement('div');
      popup.className = 'add-task-popup';
      popup.dataset.columnId = colId;
      popup.setAttribute('role', 'dialog');
      popup.setAttribute('aria-label', 'Add task');
      popup.appendChild(frag);
      document.body.appendChild(popup);
      makeDraggable(popup); // KF-209
      var r = btn.getBoundingClientRect();
      placePopup(popup, r.left, r.bottom + 6);
      var input = popup.querySelector('input[name="name"]');
      if (input) input.focus();

      // Add click handler for color picker to open the named modal
      var colorPickers = popup.querySelectorAll('.color-pick-option');
      colorPickers.forEach(function(picker) {
        picker.addEventListener('click', function(e) {
          // Prevent default form submission
          e.stopPropagation();
          // Open the named color modal instead
          openNamedColorModal(form, colId);
        });
      });

      form.addEventListener('submit', function (ev) {
        ev.preventDefault();
        var submitBtn = form.querySelector('button[type="submit"]');
        if (submitBtn) submitBtn.disabled = true;
        // parse_body accepts JSON or urlencoded — not multipart.
        fetch('/api/tasks', {
          method: 'POST',
          headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
          body: new URLSearchParams(new FormData(form)),
          credentials: 'same-origin'
        })
          .then(function (resp) {
            if (!resp.ok) return resp.text().then(function (t) { throw new Error(t || ('HTTP ' + resp.status)); });
            return resp.text();
          })
          .then(function (html) {
            // Insert the new card into the column's first visible task-list
            // (unhiding per KF-206 when every lane is folded).
            var list = document.querySelector('.task-list[data-column-id="' + cssEscape(colId) + '"]:not([hidden])')
              || document.querySelector('.task-list[data-column-id="' + cssEscape(colId) + '"]');
            if (list) {
              if (list.hasAttribute('hidden')) list.removeAttribute('hidden');
              list.insertAdjacentHTML('afterbegin', html);
              if (window.htmx) window.htmx.process(list);
              refreshCardSecStates(); // KF-224: new card honors section state
            }
            updateColumnCount(colId, 1);
            popup.remove();
          })
          .catch(function (err) {
            if (submitBtn) submitBtn.disabled = false;
            var msg = popup.querySelector('.add-task-error');
            if (!msg) {
              msg = document.createElement('div');
              msg.className = 'add-task-error';
              msg.setAttribute('role', 'alert');
              form.appendChild(msg);
            }
            msg.textContent = 'Could not add task: ' + (err.message || err);
          });
      });
    });

    document.addEventListener('click', function (e) {
      if (e.target.closest('[data-cancel-add]')) {
        var popup = e.target.closest('.add-task-popup');
        if (popup) popup.remove();
        return;
      }
      // Dismiss the popup on outside click — but not when the click is the
      // tail of a drag (e.g. a popup drag or text selection released outside
      // fires click on the common ancestor, usually body).
      var open = document.querySelector('.add-task-popup');
      if (open && !e.target.closest('.add-task-popup') && !e.target.closest('[data-add-task-for]')) {
        if (lastMouseDownPos) {
          var mdx = e.clientX - lastMouseDownPos.x, mdy = e.clientY - lastMouseDownPos.y;
          if (Math.sqrt(mdx * mdx + mdy * mdy) > 6) return;
        }
        open.remove();
      }
    });

    // KF-209: remember where each mousedown landed so the dismiss handler
    // above can tell a real click from a drag-release.
    var lastMouseDownPos = null;
    document.addEventListener('mousedown', function (e) {
      lastMouseDownPos = { x: e.clientX, y: e.clientY };
    }, true);

    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape') {
        var open = document.querySelector('.add-task-popup');
        if (open) open.remove();
      }
    });
  }

  // ---------- floating menus ----------

  var openMenu = null;

  function hideFloatingMenus() {
    var any = false;
    document.querySelectorAll('.menu-pop.menu-floating, .details-popup').forEach(function (m) {
      if (!m.hidden) { m.hidden = true; any = true; }
    });
    // KF-310: keep the popover triggers' aria state in sync when a floating
    // menu is closed centrally (e.g. via the unified Escape handler).
    // KF-236: the reports button joins the same sync.
    ['notifications-btn', 'account-btn', 'topbar-reports-btn'].forEach(function (id) {
      var b = document.getElementById(id);
      if (b && b.getAttribute('aria-expanded') === 'true') {
        b.setAttribute('aria-expanded', 'false');
        any = true;
      }
    });
    if (openMenu) { openMenu = null; any = true; }
    return any;
  }

  function placeMenu(menu, x, y) {
    hideFloatingMenus();
    positionMenu(menu, x, y);
    openMenu = menu;
  }

  // Show `menu` at (x, y), clamped to the viewport; other menus stay open.
  function positionMenu(menu, x, y) {
    menu.hidden = false;
    menu.style.left = '0px';
    menu.style.top = '0px';
    var w = menu.offsetWidth;
    var h = menu.offsetHeight;
    menu.style.left = Math.max(4, Math.min(x, window.innerWidth - w - 4)) + 'px';
    menu.style.top = Math.max(4, Math.min(y, window.innerHeight - h - 4)) + 'px';
  }

  function closeAllTmMenus() {
    var any = false;
    document.querySelectorAll('.tm-menu').forEach(function (m) {
      if (!m.hidden) { m.hidden = true; any = true; }
    });
    return any;
  }

  var colMenuColumnId = null;
  var laneMenuSwimlaneId = null;

  function initBoardMenus() {
    // KF-038/KF-230: no ⋮ button on column headers (KanbanFlow parity) —
    // the column menu (Edit / Collapse / Show details / Delete) opens on
    // right-click. The lane ⋮ menu is unchanged.
    document.addEventListener('click', function (e) {
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
      // KF-038/KF-230: no ⋮ button on column headers (KanbanFlow parity) —
      // the 4-item column menu (Edit / Collapse / Show details / Delete)
      // opens on right-click.
      var th = e.target.closest('.columnHeader');
      if (!th) return;
      e.preventDefault();
      colMenuColumnId = th.dataset.columnId;
      // KF-223: the Collapse item reads "Expand" for an already-collapsed column.
      var collapseBtn = document.getElementById('column-menu').querySelector('[data-col-act="collapse"]');
      if (collapseBtn) collapseBtn.textContent = th.dataset.collapsed === '1' ? 'Expand' : 'Collapse';
      placeMenu(document.getElementById('column-menu'), e.clientX, e.clientY);
    });

    document.getElementById('column-menu').addEventListener('click', function (e) {
      var btn = e.target.closest('[data-col-act]');
      if (!btn || !colMenuColumnId) return;
      var act = btn.getAttribute('data-col-act');
      var id = colMenuColumnId;
      hideFloatingMenus();
      if (act === 'edit') openEditColumnDialog(id);
      else if (act === 'collapse') toggleColumnCollapsed(id);
      else if (act === 'details') showColumnDetails(id);
      else if (act === 'delete') deleteColumn(id);
    });

    // KF-223: flip a column's collapsed state (persisted per board via the
    // column PATCH endpoint), then reload so the strip renders.
    function toggleColumnCollapsed(id) {
      var th = document.querySelector('.columnHeader[data-column-id="' + id + '"]');
      var collapse = !(th && th.dataset.collapsed === '1');
      api('/api/columns/' + encodeURIComponent(id), 'PATCH', { collapsed: collapse })
        .then(function (res) {
          // Drain the body first so the reload does not abort it.
          return res.text().then(function () {
            if (res.ok) window.location.reload();
            else toast('Could not update the column.');
          });
        })
        .catch(function () { toast('Could not update the column.'); });
    }

    // KF-223: "Show details" — popover centred under the column header,
    // its top edge on the header's bottom rule.
    function showColumnDetails(id) {
      var th = document.querySelector('.columnHeader[data-column-id="' + id + '"]');
      if (!th) return;
      var pop = document.getElementById('column-details-popup');
      document.getElementById('details-col-name').textContent = 'Column: ' + (th.dataset.columnName || '');
      document.getElementById('details-col-count').textContent = th.dataset.taskCount || '0';
      pop.hidden = false;
      var r = th.getBoundingClientRect();
      placePopup(pop, Math.round(r.left + (r.width - pop.offsetWidth) / 2), Math.round(r.bottom));
      openMenu = pop;
    }

    document.getElementById('details-col-close').addEventListener('click', function () {
      document.getElementById('column-details-popup').hidden = true;
      if (openMenu && openMenu.id === 'column-details-popup') openMenu = null;
    });

    // KF-223: clicking anywhere on a collapsed strip expands the column
    // (KanbanFlow parity). Right-click still opens the column menu.
    document.getElementById('column-headers-row').addEventListener('click', function (e) {
      var th = e.target.closest('.columnHeader--collapsed');
      if (!th || e.target.closest('button')) return;
      toggleColumnCollapsed(th.dataset.columnId);
    });
    // KF-223: the strip's body cells expand it too.
    document.querySelectorAll('.board-table tbody').forEach(function (body) {
      body.addEventListener('click', function (e) {
        var td = e.target.closest('td.board-cell--collapsed');
        if (td) toggleColumnCollapsed(td.dataset.columnId);
      });
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

    // KF-334: the card's column comes from its list; cards carry no column id.
    function cardColumnId(card) {
      var list = card ? card.closest('.task-list') : null;
      return list ? list.dataset.columnId : null;
    }


    function hideCardSubmenu() {
      var sub = document.getElementById('card-ctx-submenu');
      if (sub) { sub.hidden = true; sub.innerHTML = ''; }
    }

    // KF-334: the submenu opens beside the card menu, which stays open
    // (KanbanFlow shows both side by side).
    function openCardSubmenu(btn, buildItems) {
      var sub = document.getElementById('card-ctx-submenu');
      sub.innerHTML = '';
      buildItems(sub);
      var r = btn.getBoundingClientRect();
      positionMenu(sub, r.right + 2, r.top - 6);
      // Flip to the left when the submenu would run off the viewport.
      var sr = sub.getBoundingClientRect();
      if (r.right + 2 + sr.width > window.innerWidth - 4) {
        positionMenu(sub, r.left - sr.width - 2, r.top - 6);
      }
    }

    // KF-334: Move submenu icons (16px line icons like the card menu's).
    var CARD_SUB_ICONS = {
      'move-right': '<path d="M2.5 8h11M9.5 4l4 4-4 4"/>',
      'move-bottom': '<path d="M8 2v9M4.5 7.5L8 11l3.5-3.5M3 14h10"/>',
      'move-advanced': '<path d="M9 2.5h4.5V7M13.5 2.5L7.5 8.5"/><path d="M11.5 9.5v3a1 1 0 0 1-1 1h-7a1 1 0 0 1-1-1v-7a1 1 0 0 1 1-1h3"/>',
    };

    function cardSubmenuButton(label, subAct, extra) {
      var b = document.createElement('button');
      b.type = 'button';
      if (CARD_SUB_ICONS[subAct]) {
        b.innerHTML = '<span class="menu-item-icon" aria-hidden="true"><svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">' +
          CARD_SUB_ICONS[subAct] + '</svg></span>';
      }
      b.appendChild(document.createTextNode(label));
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
        // KF-329: KanbanFlow's "Start timer" is a direct action — no submenu,
        // no "Select in timer" entry.
        hideCardSubmenu();
        hideFloatingMenus();
        if (typeof TimerUI !== 'undefined' && TimerUI.startForTask) {
          TimerUI.startForTask(task.id, task.name);
        } else {
          toast('Timer is not available on this page.');
        }
      } else if (act === 'move') {
        openCardSubmenu(btn, function (sub) {
          // KF-334: KanbanFlow-style relative-move submenu.
          var card = cardMenuCard;
          if (!card) return;
          var currentColumnId = cardColumnId(card);
          var headers = Array.prototype.slice.call(document.querySelectorAll('#column-headers-row .columnHeader'));
          var currentIndex = headers.findIndex(function (h) { return h.dataset.columnId === currentColumnId; });
          // "Move right" only when a column exists to the right.
          if (currentIndex >= 0 && currentIndex < headers.length - 1) {
            var nextColumnId = headers[currentIndex + 1].dataset.columnId;
            sub.appendChild(cardSubmenuButton('Move right', 'move-right', { 'data-column-id': nextColumnId }));
          }
          // "Move to bottom" of the card's own column, in every column.
          sub.appendChild(cardSubmenuButton('Move to bottom', 'move-bottom'));
          // "Move advanced" (always available).
          sub.appendChild(cardSubmenuButton('Move advanced', 'move-advanced'));
        });
      } else if (act === 'color') {
        openCardSubmenu(btn, function (sub) {
          var src = document.getElementById('board-colors');
          if (src) {
            Array.prototype.forEach.call(src.querySelectorAll('span[data-id]'), function (s) {
              var b = cardSubmenuButton(s.dataset.label || s.dataset.value, 'color',
                { 'data-color-id': s.dataset.id, 'data-color-value': s.dataset.value,
                  // KF-142: carry the human label so the card's title can be re-synced.
                  'data-color-label': s.dataset.label || '' });
              b.style.borderLeft = '0.9rem solid ' + (s.dataset.bg || '#fff');
              sub.appendChild(b);
            });
          }
        });
      } else {
        hideCardSubmenu();
        hideFloatingMenus();
        if (act === 'assign-members') openMembersDialog(task ? task.id : null);
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
      if (sub === 'move-right') {
        // KF-334: move to the end of the column to the right.
        var colId = btn.getAttribute('data-column-id');
        var curList = cardMenuCard ? cardMenuCard.closest('.task-list') : null;
        var curLane = curList ? (curList.dataset.swimlaneId || null) : null;
        var listSel = '.task-list[data-column-id="' + cssEscape(colId) + '"]';
        var targetList = (curLane && document.querySelector(listSel + '[data-swimlane-id="' + cssEscape(curLane) + '"]')) ||
            document.querySelector(listSel);
        var laneId = targetList ? (targetList.dataset.swimlaneId || null) : curLane;
        var position = targetList ? targetList.querySelectorAll('.task-card').length : 0;
        api('/api/tasks/' + encodeURIComponent(task.id) + '/move', 'POST',
            { column_id: colId, swimlane_id: laneId, position: position })
          .then(function (res) {
            if (res.ok) window.location.reload();
            else toast('Could not move task.');
          });
      } else if (sub === 'move-bottom') {
        // KF-334: move to the bottom of the current column.
        var curList = cardMenuCard ? cardMenuCard.closest('.task-list') : null;
        var curLane = curList ? (curList.dataset.swimlaneId || null) : null;
        var colId = cardColumnId(cardMenuCard);
        var listSel = '.task-list[data-column-id="' + cssEscape(colId) + '"]';
        var targetList = (curLane && document.querySelector(listSel + '[data-swimlane-id="' + cssEscape(curLane) + '"]')) ||
            document.querySelector(listSel);
        var laneId = targetList ? (targetList.dataset.swimlaneId || null) : curLane;
        var position = targetList ? targetList.querySelectorAll('.task-card').length : 0;
        api('/api/tasks/' + encodeURIComponent(task.id) + '/move', 'POST',
            { column_id: colId, swimlane_id: laneId, position: position })
          .then(function (res) {
            if (res.ok) window.location.reload();
            else toast('Could not move task.');
          });
      } else if (sub === 'move-advanced') {
        // KF-334/KF-141: the "Move task" popover, anchored at the card.
        openMoveDialog(task.id, cardMenuCard);
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
            // KF-142: the card's title is the color label — re-sync it so the
            // accessible title doesn't go stale after a recolor.
            var colorLabel = btn.getAttribute('data-color-label');
            if (colorLabel) cardMenuCard.title = colorLabel;
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
      // KF-184: styled in-page confirmation (KanbanFlow parity); the native
      // window.confirm() auto-dismisses under automation, silently no-op'ing.
      showConfirmDialog('Delete task', 'Delete "' + task.name + '"?', 'Delete', function () {
        api('/api/tasks/' + encodeURIComponent(task.id), 'DELETE')
          .then(function (res) {
            if (!res.ok) { toast('Could not delete task.'); return; }
            // KF-193: the header count went stale after context-menu deletes —
            // capture the column before detaching the card, then refresh it
            // (mirrors the add-task and drag/drop paths via updateColumnCount).
            var list = cardMenuCard ? cardMenuCard.closest('.task-list') : null;
            var colId = list ? list.dataset.columnId : null;
            if (cardMenuCard && cardMenuCard.parentNode) cardMenuCard.remove();
            cardMenuCard = null;
            if (colId) updateColumnCount(colId, -1);
          });
      });
    }

    // KF-122: toolbar buttons removed; guard for null (layout view calls
    // openAddColumnDialog/addSwimlane directly).
    var addColBtn = document.getElementById('add-column-btn');
    if (addColBtn) addColBtn.addEventListener('click', function () {
      openAddColumnDialog('end');
    });
    var addSwimBtn = document.getElementById('add-swimlane-btn');
    if (addSwimBtn) addSwimBtn.addEventListener('click', function () {
      addSwimlane();
    });
    document.getElementById('ac-add').addEventListener('click', doAddColumn);
    // KF-156: swimlane dialog wiring.
    var swSaveBtn = document.getElementById('sw-save');
    if (swSaveBtn) swSaveBtn.addEventListener('click', doSaveSwimlane);
    var swNameInput = document.getElementById('sw-name');
    if (swNameInput) swNameInput.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') { e.preventDefault(); doSaveSwimlane(); }
    });
    document.getElementById('ec-save').addEventListener('click', doSaveColumn);
    document.getElementById('mt-move-btn').addEventListener('click', doMoveTask);
    // KF-334: a click outside the card-anchored popover closes it.
    document.getElementById('move-task-dialog').addEventListener('click', function (e) {
      if (e.target === e.currentTarget && e.currentTarget.classList.contains('dlg-overlay--popover')) {
        e.currentTarget.hidden = true;
      }
    });
    document.getElementById('est-add').addEventListener('click', doAddEstimate);
    // KF-122: save-template toolbar button removed; guard for null.
    // KF-163: the board menu now opens the same dialog via openSaveTemplateDialog().
    var saveTplBtn = document.getElementById('save-template-btn');
    if (saveTplBtn) saveTplBtn.addEventListener('click', openSaveTemplateDialog);
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

  // KF-047: styled confirmation dialog replacing window.confirm().
  // Calls `onConfirm` when the user clicks the confirm button.
  function showConfirmDialog(title, message, okLabel, onConfirm) {
    document.getElementById('confirm-title').textContent = title;
    document.getElementById('confirm-message').textContent = message;
    var okBtn = document.getElementById('confirm-ok');
    okBtn.textContent = okLabel || 'Delete';
    // Replace the button to drop any previously attached listeners.
    var fresh = okBtn.cloneNode(true);
    okBtn.parentNode.replaceChild(fresh, okBtn);
    fresh.addEventListener('click', function () {
      document.getElementById('confirm-dialog').hidden = true;
      onConfirm();
    });
    document.getElementById('confirm-dialog').hidden = false;
    fresh.focus();
  }

  // Generic dialog wiring on every page: [data-close-dialog] hides its overlay.
  document.addEventListener('click', function (e) {
    var closer = e.target.closest('[data-close-dialog]');
    if (closer) {
      var overlay = closer.closest('.dlg-overlay');
      if (overlay) overlay.hidden = true;
    }
  });

  // KF-304/KF-305: delete-board page — styled delete confirm and Archive board.
  var boardDeleteInitialized = false;
  function initBoardDeletePage() {
    var section = document.querySelector('.delete-board-confirm');
    if (!section || boardDeleteInitialized) return;
    boardDeleteInitialized = true;
    var id = section.dataset.boardId;
    var name = section.dataset.boardName;
    var delBtn = document.getElementById('delete-board-confirm-btn');
    var arcBtn = document.getElementById('archive-board-btn');
    var err = document.getElementById('delete-board-error');
    function showError(msg) {
      err.textContent = msg;
      err.hidden = false;
    }
    function failOnError(res) {
      if (res.ok) return res;
      return res.text().then(function (t) { throw new Error(t || ('HTTP ' + res.status)); });
    }
    arcBtn.addEventListener('click', function () {
      arcBtn.disabled = true;
      api('/api/boards/' + encodeURIComponent(id) + '/config', 'PUT', { archived: true })
        .then(failOnError)
        .then(function () { window.location.href = '/boards/archived'; })
        .catch(function (ex) {
          arcBtn.disabled = false;
          showError('Could not archive board: ' + ex.message);
        });
    });
    delBtn.addEventListener('click', function () {
      if (section.dataset.lastBoard) {
        showError('Could not delete board: cannot delete the last board');
        return;
      }
      err.hidden = true;
      showConfirmDialog('Delete board', 'Delete the board "' + name + '"? This cannot be undone.', 'Delete', function () {
        delBtn.disabled = true;
        api('/api/boards/' + encodeURIComponent(id), 'DELETE')
          .then(failOnError)
          .then(function () { window.location.href = '/boards/new'; })
          .catch(function (ex) {
            delBtn.disabled = false;
            showError('Could not delete board: ' + ex.message);
          });
      });
    });
  }

  // KF-306: column-delete confirmation with verbatim KanbanFlow text and the
  // "Move the tasks to the column" move-target option (GM-164). Deleting a
  // non-empty column requires choosing a move target — no "Delete anyway".
  function deleteColumn(id) {
    var th = document.querySelector('.columnHeader[data-column-id="' + cssEscape(id) + '"]');
    var count = th ? parseInt(th.dataset.taskCount, 10) || 0 : 0;

    // Collect the other columns as move targets.
    var headers = Array.prototype.slice.call(document.querySelectorAll('#column-headers-row .columnHeader'));
    var targets = headers
      .filter(function (h) { return h.dataset.columnId !== id; })
      .map(function (h) { return { id: h.dataset.columnId, name: h.dataset.columnName }; });

    // Build a dedicated dialog (the generic showConfirmDialog has no room for
    // the checkbox + target dropdown).
    var overlay = document.createElement('div');
    overlay.className = 'dlg-overlay';
    overlay.innerHTML =
      '<div class="dlg dlg-light" role="dialog" aria-label="Delete column">' +
        '<div class="dlg-title">Delete column</div>' +
        '<p class="dlg-text">Are you sure you want to delete this column?</p>' +
        (count > 0 ?
          '<label class="dlg-field" style="display:flex;align-items:center;gap:8px;margin:12px 0;">' +
            '<input type="checkbox" id="dc-move-check">' +
            '<span>Move the tasks to the column</span>' +
          '</label>' +
          '<label class="dlg-field" id="dc-target-wrap" style="display:none;">' +
            '<select id="dc-target" aria-label="Move tasks to column">' +
              targets.map(function (t) {
                return '<option value="' + escapeHtml(t.id) + '">' + escapeHtml(t.name) + '</option>';
              }).join('') +
            '</select>' +
          '</label>'
        : '') +
        '<div class="dlg-actions">' +
          '<button type="button" class="btn" id="dc-cancel">Cancel</button>' +
          '<button type="button" class="btn btn-danger" id="dc-ok">Delete</button>' +
        '</div>' +
      '</div>';
    document.body.appendChild(overlay);

    var cleanup = function () { if (overlay.parentNode) overlay.parentNode.removeChild(overlay); };
    overlay.querySelector('#dc-cancel').addEventListener('click', cleanup);
    overlay.addEventListener('click', function (e) { if (e.target === overlay) cleanup(); });

    var moveCheck = overlay.querySelector('#dc-move-check');
    var targetWrap = overlay.querySelector('#dc-target-wrap');
    if (moveCheck) {
      moveCheck.addEventListener('change', function () {
        targetWrap.style.display = moveCheck.checked ? '' : 'none';
      });
    }

    overlay.querySelector('#dc-ok').addEventListener('click', function () {
      var doDelete = function () {
        api('/api/columns/' + encodeURIComponent(id), 'DELETE')
          .then(function (res) {
            if (res.ok) window.location.reload();
            else toast('Could not delete column.');
          });
      };

      if (count === 0) {
        cleanup();
        doDelete();
        return;
      }

      // Non-empty column: a move target is required (GM-164 — no "Delete anyway").
      if (!moveCheck || !moveCheck.checked) {
        toast('Choose a column to move the tasks to.');
        return;
      }
      var targetSel = overlay.querySelector('#dc-target');
      var targetId = targetSel ? targetSel.value : '';
      if (!targetId) {
        toast('Choose a column to move the tasks to.');
        return;
      }

      // Move every task in this column (across all swimlanes) to the target,
      // then delete the column.
      var cards = document.querySelectorAll(
        '.task-list[data-column-id="' + cssEscape(id) + '"] .task-card[data-task-id]'
      );
      var taskIds = Array.prototype.map.call(cards, function (c) { return c.dataset.taskId; });
      cleanup();

      var chain = Promise.resolve();
      taskIds.forEach(function (taskId) {
        chain = chain.then(function () {
          return api('/api/tasks/' + encodeURIComponent(taskId) + '/move', 'POST', { column_id: targetId });
        });
      });
      chain.then(doDelete).catch(function () { toast('Could not move tasks.'); });
    });

    overlay.querySelector('#dc-ok').focus();
  }

  // KF-041: placement is 'beginning', 'end', or { anchor: <column id>,
  // side: 'left'|'right' } to insert the new column adjacent to the column
  // whose menu opened the dialog.
  var addColumnPlacement = 'end';

  function openAddColumnDialog(placement) {
    addColumnPlacement = placement || 'end';
    document.getElementById('ac-name').value = '';
    var posLabel = document.getElementById('ac-position-label');
    var posHint = document.getElementById('ac-position-hint');
    if (placement && typeof placement === 'object') {
      posLabel.hidden = true;
      posHint.hidden = false;
      var anchorTh = document.querySelector('.columnHeader[data-column-id="' + cssEscape(placement.anchor) + '"]');
      var anchorName = anchorTh ? anchorTh.dataset.columnName : '';
      posHint.textContent = placement.side === 'left'
        ? 'The column will be added to the left of "' + anchorName + '".'
        : 'The column will be added to the right of "' + anchorName + '".';
    } else {
      posLabel.hidden = false;
      posHint.hidden = true;
      document.getElementById('ac-position').value = placement === 'beginning' ? 'beginning' : 'end';
    }
    document.getElementById('add-column-dialog').hidden = false;
    document.getElementById('ac-name').focus();
  }

  function doAddColumn() {
    var name = document.getElementById('ac-name').value.trim();
    if (!name) { toast('Column name is required.'); return; }
    // KF-181: honor the Position combobox's CURRENT value. The dialog may
    // have been opened with one placement and the user may have changed the
    // combobox since; the stale dialog-open-time value must not win. The
    // anchor-object placement (label hidden, hint shown) bypasses this.
    var placement = addColumnPlacement;
    if (placement === null || typeof placement !== 'object') {
      var posLabel = document.getElementById('ac-position-label');
      var posSel = document.getElementById('ac-position');
      if (posSel && posLabel && !posLabel.hidden) placement = posSel.value;
    }
    api('/api/columns', 'POST', { board_id: boardId(), name: name })
      .then(function (res) { return res.ok ? res.json() : Promise.reject(new Error('create')); })
      .then(function (data) {
        if (!data || !data.id) return null;
        if (placement && typeof placement === 'object') {
          // Adjacent insert: the new column lands at the end, so move it
          // next to the anchor column using the header row's DOM order.
          var headers = Array.prototype.slice.call(
            document.querySelectorAll('#column-headers-row .columnHeader'));
          var anchorIdx = -1;
          for (var i = 0; i < headers.length; i++) {
            if (headers[i].dataset.columnId === placement.anchor) { anchorIdx = i; break; }
          }
          if (anchorIdx < 0) return null;
          var target = placement.side === 'left' ? anchorIdx : anchorIdx + 1;
          return api('/api/columns/' + encodeURIComponent(data.id) + '/move', 'POST', { position: target });
        }
        if (placement === 'beginning') {
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

  function columnConfigBag(th) {
    var cfg = {};
    if (th && th.dataset.columnConfig) {
      try { cfg = JSON.parse(th.dataset.columnConfig) || {}; } catch (e) { cfg = {}; }
    }
    return cfg;
  }

  function columnPropConfig(th) {
    var cfg = columnConfigBag(th);
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
    // KF-042: the remaining fields live in the column's opaque config_json
    // bag — load the saved values so editing a name doesn't wipe them.
    var cfg = columnConfigBag(th);
    document.getElementById('ec-sorting').value = cfg.sorting || 'none';
    // KF-045: column sum is a dropdown ("None"/"Show sum"), not a checkbox.
    document.getElementById('ec-column-sum').value = cfg.column_sum ? 'sum' : 'none';
    document.getElementById('ec-group-by-date').checked = !!cfg.group_by_date || (th.dataset.columnName && th.dataset.columnName.toLowerCase().includes('done'));
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
      // KF-045: dropdown value mapped back to the boolean config bag.
      column_sum: document.getElementById('ec-column-sum').value === 'sum',
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

  // KF-156: add/rename use a real in-page dialog (KanbanFlow parity) instead
  // of window.prompt, which automation environments auto-dismiss.
  var swimlaneDialogMode = 'add'; // 'add' | 'rename'
  var swimlaneRenameId = null;

  function openSwimlaneDialog(mode, id, currentName) {
    swimlaneDialogMode = mode;
    swimlaneRenameId = id || null;
    document.getElementById('sw-name-title').textContent =
      mode === 'add' ? 'Add swimlane' : 'Rename swimlane';
    document.getElementById('sw-save').textContent =
      mode === 'add' ? 'Add' : 'Save';
    document.getElementById('sw-name').value = currentName || '';
    document.getElementById('swimlane-dialog').hidden = false;
    document.getElementById('sw-name').focus();
  }

  function doSaveSwimlane() {
    var name = document.getElementById('sw-name').value.trim();
    if (!name) { toast('Swimlane name is required.'); return; }
    document.getElementById('swimlane-dialog').hidden = true;
    if (swimlaneDialogMode === 'add') {
      api('/api/swimlanes', 'POST', { board_id: boardId(), name: name })
        .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not add swimlane.'); });
    } else if (swimlaneRenameId) {
      api('/api/swimlanes/' + encodeURIComponent(swimlaneRenameId), 'PATCH', { name: name })
        .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not rename swimlane.'); });
    }
  }

  function addSwimlane() {
    openSwimlaneDialog('add');
  }

  function renameSwimlane(id) {
    var label = document.querySelector('.swimlane-label[data-swimlane-id="' + cssEscape(id) + '"]');
    var current = label ? label.dataset.swimlaneName : '';
    openSwimlaneDialog('rename', id, current);
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
    // KF-160: styled in-page confirmation (KanbanFlow parity); window.confirm()
    // is auto-dismissed by automation runtimes, so Delete silently did nothing.
    showConfirmDialog('Delete swimlane', msg, 'Delete', function () {
      api('/api/swimlanes/' + encodeURIComponent(id), 'DELETE')
        .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not delete swimlane.'); });
    });
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

  // KF-163: opened from the board menu (KanbanFlow parity).
  function openSaveTemplateDialog() {
    document.getElementById('st-name').value = '';
    document.getElementById('st-description').value = '';
    document.getElementById('save-template-dialog').hidden = false;
    document.getElementById('st-name').focus();
  }

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

  function openModal(taskId, keepDirty) {
    fetch('/api/tasks/' + encodeURIComponent(taskId) + '/modal', {
      headers: { 'Accept': 'text/html' },
    })
      .then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); })
      .then(function (html) {
        document.getElementById('modal-root').innerHTML = html;
        // A refresh must not clear unsaved-changes state (e.g. after a
        // labels/due-date save the board still needs a reload on close).
        modalDirty = !!keepDirty;
        modalSubviewOpen = null;
        // KF-250: lock the board behind the modal so it cannot scroll.
        document.body.classList.add('modal-open');
        wireModal();
      })
      .catch(function () { /* leave the board as-is on failure */ });
  }

  function refreshModal() {
    var id = modalTaskId();
    if (id) openModal(id, modalDirty);
  }

  // Which dedicated sub-view (KF-100 History / KF-101 time log) is
  // currently rendered inside the modal body, or null for the task view.
  var modalSubviewOpen = null;

  // Dedicated in-modal sub-views (KanbanFlow parity): the full time log
  // (day-grouped, member/avatar, delete, 12h ranges) and the History
  // activity trail. Rendered into the modal body; the back button
  // (data-tm-subview="back") reloads the task modal.
  function openModalSubview(kind) {
    var id = modalTaskId();
    if (!id) return;
    closeAllTmMenus();
    var url = '/api/tasks/' + encodeURIComponent(id) + '/' +
      (kind === 'time-log' ? 'time-log-view' : 'history-view');
    fetch(url, { headers: { 'Accept': 'text/html' } })
      .then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); })
      .then(function (html) {
        var body = document.querySelector('#modal-root .task-modal-body');
        if (body) {
          body.innerHTML = html;
          modalSubviewOpen = kind;
          modalDirty = true;
        }
      })
      .catch(function () {
        toast('Could not load ' + (kind === 'time-log' ? 'time log' : 'history') + '.');
      });
  }

  // Refresh the dedicated time-log sub-view when it is open (the
  // compact inline log is gone; the Time spent row links to the sub-view).
  function refreshModalLog() {
    var id = modalTaskId();
    if (!id) return;
    if (modalSubviewOpen === 'time-log') openModalSubview('time-log');
  }

  function closeModal() {
    var root = document.getElementById('modal-root');
    if (!root) return;
    root.innerHTML = '';
    modalSubviewOpen = null;
    document.body.classList.remove('modal-open'); // KF-250: release the board scroll lock.
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
          // KF-185: the rename no longer reloads the board (the save is
          // async, so closeModal on Ctrl+Enter races it), so update the
          // card DOM in place — title text and the data attribute the
          // context menu reads.
          var card = document.querySelector('.task-card[data-task-id="' + cssEscape(id) + '"]');
          if (card) {
            var title = card.querySelector('.card-title');
            if (title) title.textContent = name;
            card.dataset.taskName = name;
          }
        }
      });
  }

  // KF-253: the description is a static text block (click-to-edit); edits
  // go through the description editor dialog, never an inline textarea.
  function openDescriptionEditor() {
    var id = modalTaskId();
    if (!id) return;
    var wrap = document.getElementById('modal-description-wrap');
    var input = document.getElementById('desc-editor-input');
    if (!wrap || !input) return;
    input.value = wrap.dataset.description || '';
    document.getElementById('description-dialog').hidden = false;
    input.focus();
  }

  function saveDescriptionEditor() {
    var id = modalTaskId();
    var wrap = document.getElementById('modal-description-wrap');
    var input = document.getElementById('desc-editor-input');
    if (!id || !wrap || !input) return;
    var value = input.value;
    api('/api/tasks/' + encodeURIComponent(id), 'PATCH', { description: value })
      .then(function (res) { if (!res.ok) throw new Error('save failed'); })
      .then(function () {
        modalDirty = true;
        document.getElementById('description-dialog').hidden = true;
        wrap.dataset.description = value;
        var block = document.getElementById('modal-description-text');
        if (block) {
          if (value.trim() === '') {
            block.textContent = 'Add description…';
            block.classList.add('tm-description-empty');
            block.setAttribute('aria-label', 'Add description');
            block.title = 'Click to add a description';
          } else {
            block.textContent = value;
            block.classList.remove('tm-description-empty');
            block.setAttribute('aria-label', 'Edit description');
            block.title = 'Click to edit description';
          }
        }
        toast('Description saved.');
      })
      .catch(function () { toast('Could not save description.'); });
  }

  function deleteModalTask() {
    var id = modalTaskId();
    if (!id) return;
    // KF-184: styled in-page confirmation (KanbanFlow parity); the native
    // window.confirm() auto-dismisses under automation, silently no-op'ing.
    showConfirmDialog('Delete task', 'Delete this task and its time entries?', 'Delete', function () {
      api('/api/tasks/' + encodeURIComponent(id), 'DELETE')
        .then(function (res) {
          if (res.ok) { modalDirty = true; closeModal(); }
        });
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
      b.dataset.defaultColor = s.dataset.default;
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
        // KF-257: keep the Color row's dot + name text in sync with the pick.
        // The board's default color renders dot-only (no name), matching the
        // server-side contract; any other color renders dot + name.
        var label = btn.title || '';
        var isDefault = btn.dataset.defaultColor === 'true';
        var chip = modal ? modal.querySelector('.tm-color-chip') : null;
        if (chip) chip.title = (!isDefault && label) ? label + ' — click to change' : 'Click to change';
        var nameEl = modal ? modal.querySelector('.tm-color-name') : null;
        if (isDefault) {
          if (nameEl) nameEl.remove();
        } else {
          if (!nameEl && modal && chip) {
            // Default (uncolored) tasks render no name span; create it the
            // first time a non-default color is picked so the name appears live.
            nameEl = document.createElement('span');
            nameEl.className = 'tm-color-name';
            chip.parentNode.appendChild(nameEl);
          }
          if (nameEl) nameEl.textContent = label;
        }
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

  function openNamedColorModal(form, colId) {
    // Create the modal if it doesn't exist
    var modal = document.getElementById('quick-add-color-modal');
    if (!modal) {
      // Modal already created in HTML template, but just in case
      return;
    }

    // Get the board colors
    var boardColors = document.getElementById('board-colors');
    if (!boardColors) return;

    // Get the current selected color from the form
    var currentColorId = form.querySelector('input[name="color_id"]:checked')?.value || null;

    // Clear existing options
    var list = document.getElementById('quick-add-color-list');
    if (!list) return;
    list.innerHTML = '';

    // Create color options
    Array.prototype.forEach.call(boardColors.querySelectorAll('span[data-id]'), function (colorSpan) {
      var option = document.createElement('div');
      option.className = 'quick-add-color-option';
      option.setAttribute('role', 'option');
      option.setAttribute('aria-label', colorSpan.dataset.label);
      option.dataset.colorId = colorSpan.dataset.id;
      option.dataset.colorValue = colorSpan.dataset.value;

      // Create dot
      var dot = document.createElement('span');
      dot.className = 'color-pick-dot';
      dot.style.backgroundColor = colorSpan.dataset.bg;
      dot.style.borderColor = colorSpan.dataset.border;

      // Create name
      var name = document.createElement('span');
      name.className = 'color-name';
      name.textContent = colorSpan.dataset.label;

      // Create menu button
      var menuBtn = document.createElement('button');
      menuBtn.type = 'button';
      menuBtn.className = 'color-menu-btn';
      menuBtn.setAttribute('aria-label', 'Color options for ' + colorSpan.dataset.label);
      menuBtn.innerHTML = '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="8" r="1"/><circle cx="8" cy="14" r="1"/><circle cx="8" cy="2" r="1"/></svg>';

      option.appendChild(dot);
      option.appendChild(name);
      option.appendChild(menuBtn);

      // Set selected state
      if (currentColorId === colorSpan.dataset.id) {
        option.classList.add('selected');
      }

      // Add click handler
      option.addEventListener('click', function(e) {
        // Prevent event from bubbling up to form submission
        e.stopPropagation();
        // Update form's radio button
        var radio = form.querySelector('input[value="' + colorSpan.dataset.id + '"]');
        if (radio) {
          radio.checked = true;
          // Update selected class on all options
          list.querySelectorAll('.quick-add-color-option').forEach(opt => {
            opt.classList.remove('selected');
          });
          option.classList.add('selected');
        }
        // Close the modal after selection
        modal.hidden = true;
      });

      list.appendChild(option);
    });

    // Show modal
    modal.hidden = false;

    // Close handler
    var closeBtn = modal.querySelector('[data-close-dialog]');
    if (closeBtn) {
      closeBtn.onclick = function() {
        modal.hidden = true;
      };
    }
  }

  // KF-071: the More-menu Watch toggle is a persisted flag (KanbanFlow
  // parity), rendered by the server and mirrored here for the live toggle.
  function modalWatchedState() {
    var btn = document.getElementById('tm-watch-btn');
    return !!(btn && btn.dataset.watched === 'true');
  }
  function setModalWatchUI(watched) {
    var btn = document.getElementById('tm-watch-btn');
    if (!btn) return;
    btn.dataset.watched = watched ? 'true' : 'false';
    btn.innerHTML = watched ? '&#10003; Unwatch' : 'Watch';
  }

  function modalAction(act) {
    var id = modalTaskId();
    closeAllTmMenus();
    if (act === 'pick-color') {
      // KF-257: the Color row shows dot + color name; clicking the dot toggles the full picker.
      var picker = document.getElementById('modal-color-picker');
      if (picker) picker.hidden = !picker.hidden;
    } else if (act === 'manual-time') {
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
      openModalSubview('time-log');
    } else if (act === 'history') {
      openModalSubview('history');
    } else if (act === 'print') {
      window.print();
    } else if (act === 'time-in-column') {
      // KF-275: the board-level reports menu routes the same unimplemented
      // report through reportAction's default toast; mirror it here rather
      // than invent a destination KanbanFlow never showed (GM-089 caveat).
      toast('Time in column report is not available in ChipFlow yet.');
    } else if (act === 'watch') {
      // KF-071: Watch actually toggles a persisted flag on the task
      // (KanbanFlow parity); the menu item flips Watch/Unwatch live.
      var wantWatched = !modalWatchedState();
      api('/api/tasks/' + encodeURIComponent(id) + '/watch', 'POST', { watched: wantWatched })
        .then(function (resp) {
          if (!resp.ok) throw new Error('watch failed');
          return resp.json();
        })
        .then(function (data) {
          setModalWatchUI(!!(data && data.watched));
          toast(data && data.watched ? 'Watching this task.' : 'Stopped watching this task.');
        })
        .catch(function () { toast('Could not update watch state.'); });
    } else if (act === 'task-url') {
      copyTaskUrl(id);
    } else if (act === 'copy') {
      copyModalTask();
    } else if (act === 'shortcuts') {
      document.getElementById('shortcuts-dialog').hidden = false;
    } else if (act === 'delete') {
      deleteModalTask();
    } else if (act === 'add-description') {
      // KF-253: opens the description editor dialog (the description is a
      // static text block now, never an inline textarea).
      openDescriptionEditor();
    } else if (act === 'add-member') {
      openMembersDialog(id);
    } else if (act === 'add-label') {
      // KF-144: was openLabelsDialog() (undefined) — the dialog object is LabelsDialog.
      LabelsDialog.open();
    } else if (act === 'add-subtask') {
      var subInput = document.getElementById('modal-subtask-input');
      if (subInput) { subInput.focus(); subInput.scrollIntoView({ behavior: 'smooth', block: 'center' }); }
    } else if (act === 'add-duedate') {
      // KF-145: was openDueDateDialog() (undefined) — the dialog object is DueDateDialog.
      DueDateDialog.open();
    } else if (act === 'add-comment') {
      var commentInput = document.getElementById('modal-comment-input');
      if (commentInput) { commentInput.focus(); commentInput.scrollIntoView({ behavior: 'smooth', block: 'center' }); }
    } else if (act === 'add-attachment') {
      // KF-332: the Add menu's Attachment entry is disabled (KanbanFlow
      // grays it); this branch is unreachable but kept as a guard.
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

  // KF-070/KF-334: the "Move task" dialog for `taskId` (default: the open
  // modal's task). Given `anchorCard`, it opens as a popover at that card.
  function openMoveDialog(taskId, anchorCard) {
    var id = taskId || modalTaskId();
    if (!id) return;
    moveTaskId = id;
    var card = anchorCard || document.querySelector('.task-card[data-task-id="' + cssEscape(id) + '"]');
    var list = card ? card.closest('.task-list') : null;
    var headers = Array.prototype.slice.call(document.querySelectorAll('#column-headers-row .columnHeader'));
    var boardCols = headers.map(function (h) { return { id: h.dataset.columnId, name: h.dataset.columnName }; });
    var origColId = list ? list.dataset.columnId : (boardCols[0] ? boardCols[0].id : null);
    var origLane = (list && list.dataset.swimlaneId) ? list.dataset.swimlaneId : null;
    var origCol = boardCols.filter(function (c) { return c.id === origColId; })[0];
    var origColName = origCol ? origCol.name : '';
    var currentBid = boardId();
    var boardSel = document.getElementById('mt-board');
    var colSel = document.getElementById('mt-col');

    // Lanes are per-board: keep the card's lane on this board; the server
    // picks the target board's lane on a cross-board move.
    function syncTarget() {
      moveTargetColumnId = colSel.value || null;
      moveSwimlaneId = boardSel.value === currentBid ? origLane : null;
    }
    function fillColumns(cols, selectedId) {
      colSel.innerHTML = '';
      cols.forEach(function (c) {
        var o = document.createElement('option');
        o.value = c.id;
        o.textContent = c.name;
        colSel.appendChild(o);
      });
      colSel.value = selectedId || '';
      if (!colSel.value && cols.length) colSel.value = cols[0].id;
      syncTarget();
    }

    boardSel.innerHTML = '';
    var here = document.createElement('option');
    here.value = currentBid;
    here.textContent = document.title.replace(/ — ChipFlow$/, '');
    boardSel.appendChild(here);
    fillColumns(boardCols, origColId);
    fetchJson('/api/boards').then(function (boards) {
      boardSel.innerHTML = '';
      (boards || []).forEach(function (b) {
        var o = document.createElement('option');
        o.value = b.id;
        o.textContent = b.name;
        boardSel.appendChild(o);
      });
      boardSel.value = currentBid;
    }).catch(function () { /* board list is best-effort */ });
    // Another board lists its own columns, preselecting the same name.
    boardSel.onchange = function () {
      var bid = boardSel.value;
      if (!bid || bid === currentBid) { fillColumns(boardCols, origColId); return; }
      fetchJson('/api/boards/' + encodeURIComponent(bid) + '/columns').then(function (cols) {
        if (boardSel.value !== bid) return;
        cols = cols || [];
        var same = cols.filter(function (c) { return c.name === origColName; })[0];
        fillColumns(cols, same ? same.id : null);
      }).catch(function () { toast('Could not load board columns.'); });
    };
    colSel.onchange = syncTarget;

    var overlay = document.getElementById('move-task-dialog');
    var dlg = overlay.querySelector('.dlg');
    overlay.classList.toggle('dlg-overlay--popover', !!anchorCard);
    dlg.style.left = '';
    dlg.style.top = '';
    overlay.hidden = false;
    if (anchorCard) {
      var r = anchorCard.getBoundingClientRect();
      var w = dlg.offsetWidth;
      var h = dlg.offsetHeight;
      var top = r.bottom + 4 + h <= window.innerHeight - 8 ? r.bottom + 4 : r.top - h - 4;
      dlg.style.left = Math.max(8, Math.min(r.left, window.innerWidth - w - 8)) + 'px';
      dlg.style.top = Math.max(8, Math.min(top, window.innerHeight - h - 8)) + 'px';
    }
    colSel.focus();
  }

  function doMoveTask() {
    var id = moveTaskId;
    if (!id || !moveTargetColumnId) return;
    // KF-274: no Position control — append at the end of the target cell;
    // the server clamps the index when the cell is not on this page.
    var sel = '.task-list[data-column-id="' + cssEscape(moveTargetColumnId) + '"]';
    if (moveSwimlaneId) sel += '[data-swimlane-id="' + cssEscape(moveSwimlaneId) + '"]';
    var list = document.querySelector(sel);
    var position = list ? list.querySelectorAll('.task-card').length : 1e9;
    api('/api/tasks/' + encodeURIComponent(id) + '/move', 'POST',
        { column_id: moveTargetColumnId, swimlane_id: moveSwimlaneId, position: position })
      .then(function (res) {
        document.getElementById('move-task-dialog').hidden = true;
        if (!res.ok) { toast('Could not move task.'); return; }
        if (modalTaskId() === id) { modalDirty = true; closeModal(); }
        else window.location.reload();
      });
  }

  // "Add time estimate": the estimate is stored hour-based (KanbanFlow
  // parity, KF-216); the server derives the pomodoro size from it.
  function doAddEstimate() {
    var id = modalTaskId();
    if (!id) return;
    var raw = document.getElementById('est-input').value;
    var minutes = parseEstimateMinutes(raw);
    if (!minutes || minutes <= 0) { toast('Enter an estimate like 2h, 30m, or 1h 30m.'); return; }
    var hours = Math.round((minutes / 60) * 100) / 100;
    api('/api/tasks/' + encodeURIComponent(id), 'PATCH', { estimate_hours: hours })
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
    var descBlock = document.getElementById('modal-description-text');
    if (descBlock) {
      descBlock.addEventListener('click', openDescriptionEditor);
      descBlock.addEventListener('keydown', function (e) {
        if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); openDescriptionEditor(); }
      });
    }
    // KF-254: the single-line composer submits on Enter (no Add button).
    var commentInput = document.getElementById('modal-comment-input');
    if (commentInput) commentInput.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') { e.preventDefault(); addModalComment(); }
    });
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

    // KF-278: Time spent hover menu with tooltip and action menu
    wireTimeSpentHover();
  }

  // ---------- Subtasks (KF-058) ----------
  //
  // Wired per modal open: Enter in the "Add subtask..." row creates one,
  // checkboxes toggle done, clicking a name edits it inline, and the
  // ⋮ handle opens a small menu with "Delete subtask" (KF-264), deleted
  // via the styled in-page confirm (KF-047 pattern), never window.confirm.

  function wireSubtasks() {
    var wrap = document.getElementById('modal-subtasks');
    var input = document.getElementById('modal-subtask-input');
    if (!wrap || !input) return;
    var taskId = modalTaskId();
    if (!taskId) return;

    // KF-262: keep the "N / M" count in the section header band in sync
    // whenever subtasks change (toggle, add, delete) — no page reload.
    var countEl = document.getElementById('modal-subtask-count');
    function updateSubtaskCount() {
      if (!countEl) return;
      var total = wrap.querySelectorAll('.tm-subtask').length;
      var done = wrap.querySelectorAll('.tm-subtask-check:checked').length;
      countEl.textContent = done + ' / ' + total;
    }

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
      var handle = document.createElement('button');
      handle.type = 'button';
      handle.className = 'tm-subtask-handle';
      handle.setAttribute('aria-label', 'Subtask options');
      handle.setAttribute('aria-haspopup', 'menu');
      // KF-264: muted-gray vertical ellipsis sampled from GM-085
      // (4px dots, #b1a286) replacing the old × delete button.
      handle.innerHTML = '<svg width="6" height="16" viewBox="0 0 6 16" aria-hidden="true" focusable="false">' +
        '<ellipse cx="3" cy="2.5" rx="2.0" ry="2.25" fill="#b1a286"/>' +
        '<ellipse cx="3" cy="8" rx="2.0" ry="2.25" fill="#b1a286"/>' +
        '<ellipse cx="3" cy="13.5" rx="2.0" ry="2.25" fill="#b1a286"/></svg>';
      div.appendChild(check);
      div.appendChild(name);
      div.appendChild(handle);
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
          updateSubtaskCount();
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
          updateSubtaskCount();
          modalDirty = true;
        });
    });

    // KF-264: shared per-row handle menu with "Delete subtask". Created
    // once per modal open, moved into the clicked row so it hangs off the
    // handle at the row's right edge; carries .tm-menu so click-away and
    // Escape (closeAllTmMenus) dismiss it like the other modal menus.
    var subtaskMenu = null;
    function subtaskMenuEl() {
      if (!subtaskMenu) {
        subtaskMenu = document.createElement('div');
        subtaskMenu.className = 'tm-menu tm-subtask-menu';
        subtaskMenu.hidden = true;
        subtaskMenu.setAttribute('role', 'menu');
        var item = document.createElement('button');
        item.type = 'button';
        item.textContent = 'Delete subtask';
        item.setAttribute('role', 'menuitem');
        item.addEventListener('click', function () {
          var row = subtaskMenu._row;
          closeAllTmMenus();
          if (!row || !row.isConnected) return;
          var nameEl = row.querySelector('.tm-subtask-name');
          var subName = nameEl ? nameEl.textContent.trim() : '';
          showConfirmDialog('Delete subtask', 'Delete "' + subName + '"?', 'Delete', function () {
            api('/api/tasks/' + encodeURIComponent(taskId) + '/subtasks/' +
                encodeURIComponent(row.dataset.subtaskId), 'DELETE')
              .then(function (res) {
                if (!res.ok) { toast('Could not delete subtask.'); return; }
                row.remove();
                updateSubtaskCount();
                modalDirty = true;
              });
          });
        });
        subtaskMenu.appendChild(item);
        wrap.appendChild(subtaskMenu);
      }
      return subtaskMenu;
    }

    wrap.addEventListener('click', function (e) {
      var handle = e.target.closest('.tm-subtask-handle');
      if (handle) {
        e.stopPropagation();
        var menuRow = handle.closest('.tm-subtask');
        var menu = subtaskMenuEl();
        // Toggle: same handle twice closes; another row's handle moves it.
        var wasHidden = menu.hidden || menu._row !== menuRow;
        closeAllTmMenus();
        if (wasHidden && menuRow) {
          menu._row = menuRow;
          menuRow.appendChild(menu);
          menu.hidden = false;
        } else {
          menu._row = null;
        }
        return;
      }
      var name = e.target.closest('.tm-subtask-name');
      if (name) startSubtaskEdit(name);
    });

    // KF-273: keyboard navigation and reorder for subtask rows, as documented
    // in the shortcuts dialog ("↑ ↓ — Navigate subtask list",
    // "Cmd + ↑ ↓ — Move subtask in list"). Arrow keys inside the inline
    // .tm-subtask-edit input are left alone (caret navigation).
    function moveSubtask(row, up) {
      var sibling = up ? row.previousElementSibling : row.nextElementSibling;
      while (sibling && !sibling.classList.contains('tm-subtask')) {
        sibling = up ? sibling.previousElementSibling : sibling.nextElementSibling;
      }
      if (!sibling) return;
      // Moving a focused node with insertBefore drops focus to <body> in
      // Chromium — remember it and restore focus to the moved row's name.
      var hadFocus = row.contains(document.activeElement);
      var nameEl = row.querySelector('.tm-subtask-name');
      if (up) wrap.insertBefore(row, sibling);
      else wrap.insertBefore(row, sibling.nextElementSibling);
      if (hadFocus && nameEl) nameEl.focus();
      var order = [];
      wrap.querySelectorAll('.tm-subtask').forEach(function (r) { order.push(r.dataset.subtaskId); });
      // Revert the DOM swap after a failed reorder: restore order, focus, toast.
      function revertSwap() {
        if (up) wrap.insertBefore(sibling, row);
        else wrap.insertBefore(row, sibling);
        if (hadFocus && nameEl) nameEl.focus();
        toast('Could not reorder subtask.');
      }
      api('/api/tasks/' + encodeURIComponent(taskId) + '/subtasks/order', 'PUT', { order: order })
        .then(function (res) {
          if (!res.ok) {
            // HTTP error status: revert the DOM swap on failure.
            revertSwap();
            return;
          }
          modalDirty = true;
        })
        .catch(function () {
          // Fetch-level failure (network down, aborted request): the promise
          // rejects instead of resolving, so revert the same way.
          revertSwap();
        });
    }

    wrap.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') {
        var name = e.target.closest('.tm-subtask-name');
        if (name) { e.preventDefault(); startSubtaskEdit(name); }
        return;
      }
      if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return;
      // Leave caret navigation alone while renaming inline.
      if (e.target.closest('.tm-subtask-edit')) return;
      var row = e.target.closest('.tm-subtask');
      if (!row) return;
      e.preventDefault();
      var up = e.key === 'ArrowUp';
      if (e.metaKey || e.ctrlKey) { moveSubtask(row, up); return; }
      var rows = wrap.querySelectorAll('.tm-subtask');
      var next = null;
      for (var i = 0; i < rows.length; i++) {
        if (rows[i] === row) {
          var j = i + (up ? -1 : 1);
          if (j >= 0 && j < rows.length) next = rows[j];
          break;
        }
      }
      if (next) {
        var nextName = next.querySelector('.tm-subtask-name');
        if (nextName) nextName.focus();
      }
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
      this.renderActivities();
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

    // KF-076: Interruptions tab — ⋮⋮ drag-reorder handles (SortableJS),
    // inline rename (click → yellow-highlighted input), green "Add reason"
    // button. Every mutation persists immediately via PUT /api/settings.
    renderReasons: function () {
      var list = document.getElementById('ts-reasons-list');
      if (!list || !this.settings) return;
      var reasons = this.settings.interrupt_reasons || [];
      var html = '';
      for (var i = 0; i < reasons.length; i++) {
        html += '<div class="ts-reason-row" data-index="' + i + '">' +
          '<span class="ts-drag" title="Drag to reorder" aria-label="Drag to reorder">&#8942;&#8942;</span>' +
          '<span class="ts-reason-label" data-index="' + i + '" title="Click to rename">' +
          escapeHtml(reasons[i]) + '</span>' +
          '<button type="button" class="ts-reason-remove" onclick="TimerSettings.removeReason(' + i + ')" aria-label="Remove">&times;</button></div>';
      }
      list.innerHTML = html || '<p class="settings-hint">No reasons yet.</p>';
      var self = this;
      list.querySelectorAll('.ts-reason-label').forEach(function (label) {
        label.addEventListener('click', function () { self.renameReason(label); });
      });
      if (window.Sortable && reasons.length > 1) {
        if (list._sortable) list._sortable.destroy();
        list._sortable = new Sortable(list, {
          handle: '.ts-drag',
          animation: 150,
          onEnd: function () {
            var next = [];
            list.querySelectorAll('.ts-reason-row').forEach(function (row) {
              var idx = parseInt(row.getAttribute('data-index'), 10);
              next.push(self.settings.interrupt_reasons[idx]);
            });
            self.settings.interrupt_reasons = next;
            self.saveReasons();
          }
        });
      }
    },

    // Inline rename: the label becomes a yellow-highlighted input; Enter
    // or blur commits, Esc cancels.
    renameReason: function (label) {
      var self = this;
      var i = parseInt(label.getAttribute('data-index'), 10);
      var current = (self.settings.interrupt_reasons || [])[i];
      if (current == null || label.querySelector('input')) return;
      var input = document.createElement('input');
      input.type = 'text';
      input.className = 'ts-reason-edit';
      input.value = current;
      input.maxLength = 80;
      label.textContent = '';
      label.appendChild(input);
      input.focus();
      input.select();
      var done = false;
      function commit() {
        if (done) return;
        done = true;
        var v = input.value.trim();
        if (!v) {
          // Empty rename discards the row (covers the blank Add row).
          self.settings.interrupt_reasons.splice(i, 1);
          self.renderReasons();
        } else if (v !== current) {
          self.settings.interrupt_reasons[i] = v;
          self.saveReasons();
        } else {
          self.renderReasons();
        }
      }
      input.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') commit();
        else if (e.key === 'Escape') { done = true; self.renderReasons(); }
        e.stopPropagation();
      });
      input.addEventListener('blur', commit);
    },

    // Persist the in-memory reasons order/names and refresh shared state.
    saveReasons: function () {
      var self = this;
      var saved = document.getElementById('ts-saved');
      fetch('/api/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ interrupt_reasons: self.settings.interrupt_reasons || [] })
      })
        .then(function (r) {
          if (!r.ok) throw new Error('HTTP ' + r.status);
          return r.json();
        })
        .then(function (s) {
          self.settings = s;
          self.renderReasons();
          if (saved) {
            saved.hidden = false;
            setTimeout(function () { saved.hidden = true; }, 2000);
          }
        })
        .catch(function (e) { toast('Could not save reasons: ' + e.message); });
    },

    // Green "Add reason" (KF-076): appends a blank row already in rename
    // mode; committing an empty name discards the row.
    addReason: function () {
      if (!this.settings) return;
      this.settings.interrupt_reasons = this.settings.interrupt_reasons || [];
      this.settings.interrupt_reasons.push('');
      this.renderReasons();
      var list = document.getElementById('ts-reasons-list');
      var labels = list ? list.querySelectorAll('.ts-reason-label') : [];
      var last = labels[labels.length - 1];
      if (last) this.renameReason(last);
    },

    removeReason: function (i) {
      if (!this.settings || !this.settings.interrupt_reasons) return;
      this.settings.interrupt_reasons.splice(i, 1);
      this.saveReasons();
    },

    // KF-075: break activities are persisted in settings.break_activities.
    addActivity: function () {
      var dlg = document.getElementById('activity-dialog');
      if (!dlg) { toast('Break activities are not available here.'); return; }
      document.getElementById('activity-name').value = '';
      document.getElementById('activity-desc').value = '';
      document.getElementById('activity-goal').value = '1';
      document.getElementById('activity-limit').value = '0';
      document.getElementById('activity-error').hidden = true;
      dlg.hidden = false;
    },

    closeActivityDialog: function () {
      var dlg = document.getElementById('activity-dialog');
      if (dlg) dlg.hidden = true;
    },

    confirmAddActivity: function () {
      var self = this;
      var name = document.getElementById('activity-name').value.trim();
      var desc = document.getElementById('activity-desc').value.trim();
      var goal = parseInt(document.getElementById('activity-goal').value, 10) || 0;
      var limit = parseInt(document.getElementById('activity-limit').value, 10) || 0;
      var err = document.getElementById('activity-error');
      if (!name) {
        err.textContent = 'Give the activity a name.';
        err.hidden = false;
        return;
      }
      err.hidden = true;
      var acts = (self.settings && self.settings.break_activities) || [];
      acts.push({
        id: 'act-' + Date.now().toString(36),
        name: name,
        description: desc,
        daily_goal: goal,
        daily_limit: limit > 0 ? limit : null,
      });
      fetch('/api/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ break_activities: acts })
      })
        .then(function (r) {
          if (!r.ok) throw new Error('HTTP ' + r.status);
          return r.json();
        })
        .then(function (s) {
          self.settings = s;
          self.renderActivities();
          self.closeActivityDialog();
        })
        .catch(function (e) { toast('Could not save activity: ' + e.message); });
    },

    deleteActivity: function (i) {
      var self = this;
      var acts = (self.settings && self.settings.break_activities) || [];
      if (i < 0 || i >= acts.length) return;
      acts.splice(i, 1);
      fetch('/api/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ break_activities: acts })
      })
        .then(function (r) {
          if (!r.ok) throw new Error('HTTP ' + r.status);
          return r.json();
        })
        .then(function (s) {
          self.settings = s;
          self.renderActivities();
        })
        .catch(function (e) { toast('Could not delete activity: ' + e.message); });
    },

    renderActivities: function () {
      var list = document.getElementById('ts-activities-list');
      if (!list || !this.settings) return;
      var acts = this.settings.break_activities || [];
      if (!acts.length) {
        list.innerHTML = '<p class="settings-hint">No activities yet.</p>';
        return;
      }
      var html = '';
      for (var i = 0; i < acts.length; i++) {
        var a = acts[i];
        var meta = [];
        if (a.daily_goal > 0) meta.push('goal ' + a.daily_goal + '/day');
        if (a.daily_limit != null && a.daily_limit > 0) meta.push('limit ' + a.daily_limit + '/day');
        html += '<div class="ts-activity-row">' +
          '<div class="ts-activity-main"><strong>' + escapeHtml(a.name) + '</strong>' +
          (a.description ? ' <span class="ts-activity-desc">' + escapeHtml(a.description) + '</span>' : '') +
          (meta.length ? ' <span class="ts-activity-meta">' + escapeHtml(meta.join(' · ')) + '</span>' : '') +
          '</div>' +
          '<button type="button" class="ts-activity-remove" onclick="TimerSettings.deleteActivity(' + i + ')" aria-label="Delete">&times;</button></div>';
      }
      list.innerHTML = html;
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
          if (window.TimerUI) {
            TimerUI.settings = s;
            TimerUI.syncPip();
          }
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

  // KF-129: only treat a fetch body as JSON when the server actually sent
  // JSON. `res.ok` alone is not enough: on a fresh database every /api/*
  // request 303-redirects to /setup, and fetch follows the redirect to a
  // 200-OK HTML page — calling .json() on that body throws an uncaught
  // SyntaxError ("Unexpected token '<' ... is not valid JSON").
  function jsonIfJson(res) {
    var ct = res.headers ? (res.headers.get('content-type') || '') : '';
    if (!res.ok || ct.indexOf('application/json') === -1) return null;
    return res.json();
  }

  var TimerUI = {
    settings: null,
    state: null,
    initialized: false,
    pollHandle: null,
    tickHandle: null,
    lastStatus: null,
    // KF-006: the started_at of the pomodoro whose 00:00 chime has played.
    chimedFor: null,
    // why-stop flow
    whyOriginal: null,
    whySessionId: null,
    whyEntryId: null,
    whyMode: null,
    whyTaskId: null,
    whySeconds: 0,
    whyStartWall: null,
    whyTaskName: 'Pomodoro',
    // KF-194: true while the why-stop menu is up for a session that has not
    // been finalized yet; dismissing the menu then still stops the timer.
    whyPending: false,
    // KF-078: Picture-in-Picture — a hidden canvas feeds a hidden video;
    // the video goes into a floating PiP window showing the live countdown.
    pipVideo: null,
    pipCanvas: null,

    init: function () {
      // The bootstrap block below calls init() both immediately (when the
      // deferred script runs after parsing) and on DOMContentLoaded. Guard
      // so listeners are attached exactly once: a doubled pill listener
      // toggles the popup open then shut on a single click.
      if (this.initialized) return;
      this.initialized = true;
      // KF-380: pages without the timer UI (/login, /setup) are signed out,
      // where the timer endpoints answer 401.
      if (!document.getElementById('timer-popup')) return;
      var self = this;
      this.selectedTask = this.loadSelectedTask();
      this.watchSelectedCard();
      fetch('/api/timer/settings', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then(function (res) { return jsonIfJson(res); })
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
      // KF-282: right-click context menu on the timer area (KanbanFlow
      // parity, GM-109): Comment / Label / Edit / Delete act on the
      // currently-timed task. The items are disabled while no task is
      // timed (KanbanFlow grays inapplicable items; precedent: KF-332).
      var timerMenu = document.getElementById('timer-ctx-menu');
      if (timerMenu) {
        document.addEventListener('contextmenu', function (e) {
          var inArea = e.target.closest('#timer-pill') || e.target.closest('#timer-popup');
          if (!inArea) return;
          e.preventDefault();
          var s = self.state;
          var timed = !!(s && s.phase !== 'idle' && s.taskId);
          self.timerCtxTaskId = timed ? String(s.taskId) : null;
          self.timerCtxTaskName = timed && s.taskName ? s.taskName : '';
          timerMenu.querySelectorAll('[data-timer-ctx]').forEach(function (btn) {
            btn.disabled = !timed;
          });
          placeMenu(timerMenu, e.clientX, e.clientY);
        });
        timerMenu.addEventListener('click', function (e) {
          var btn = e.target.closest('[data-timer-ctx]');
          if (!btn || btn.disabled) return;
          var act = btn.getAttribute('data-timer-ctx');
          var taskId = self.timerCtxTaskId;
          var taskName = self.timerCtxTaskName;
          hideFloatingMenus();
          if (taskId) self.timerMenuAction(act, taskId, taskName);
        });
      }
      // KF-001: in "Select task" mode the next card click picks that card
      // instead of opening it.
      document.addEventListener('click', function (e) {
        if (!self.selecting) return;
        var card = e.target.closest('.task-card');
        if (!card) return;
        e.preventDefault();
        e.stopPropagation();
        self.selectTask(card.dataset.taskId);
      }, true);
      // KF-010: the panel stays open beside a task modal (it closes only from
      // its collapse button or the pill), and its task-row link follows the
      // modal: "Select open task" while another task's modal is open.
      var modalRoot = document.getElementById('modal-root');
      if (modalRoot && window.MutationObserver) {
        new MutationObserver(function () {
          var popup = document.getElementById('timer-popup');
          if (popup && !popup.hidden) self.renderPopup();
        }).observe(modalRoot, { childList: true });
      }
      // Esc for popup/why menu is handled by the unified Escape handler.
    },

    refresh: function () {
      var self = this;
      fetch('/api/timer/status', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then(function (res) { return jsonIfJson(res); })
        .then(function (status) {
          if (!status) return;
          self.updateFromStatus(status);
          // KF-001: another tab or an API client may have deleted the
          // selected task; every status poll re-checks it.
          if (self.selectedTask && String(status.task_id || '') !== String(self.selectedTask.id)) {
            self.validateSelection();
          }
        });
    },

    updateFromStatus: function (status) {
      this.lastStatus = status;
      // KF-006: a pomodoro already at 00:00 when this page first sees it
      // (a reload) does not chime again.
      var prevStart = this.state ? this.state.startedAt : null;
      if (status.mode === 'pomodoro' && status.remaining_seconds === 0 && status.started_at !== prevStart) {
        this.chimedFor = status.started_at;
      }
      // KanbanFlow's task dot is the card's border shade.
      var cardColor = status.task_id ? this.taskInfo(status.task_id).color : null;
      this.state = {
        phase: status.phase,
        taskId: status.task_id,
        taskName: status.task_name,
        taskColor: cardColor || status.task_color,
        remainingSeconds: status.remaining_seconds,
        totalSeconds: status.total_seconds,
        startedAt: status.started_at,
        // KF-006: the server's elapsed time plus a monotonic local delta, so
        // a skewed browser clock cannot shorten or end a session.
        elapsedAtSync: status.elapsed_seconds != null ? status.elapsed_seconds
          : (status.total_seconds || 0) - (status.remaining_seconds || 0),
        syncedAt: window.performance.now(),
        mode: status.mode,
        taskUrl: status.task_url,
        sessionId: status.session_id,
        pomodoroCount: status.pomodoro_count,
      };
      // KF-001: a work session's task becomes the selected task, so it stays
      // selected after the session stops.
      if (status.phase !== 'idle' && status.task_id &&
          (status.mode === 'pomodoro' || status.mode === 'stopwatch')) {
        this.setSelectedTask({ id: String(status.task_id), name: status.task_name || '', color: this.state.taskColor });
      }
      this.renderPill();
      this.syncPip();
      this.syncTick(); // KF-197: tick runs while a session is live, popup or not
      if (document.getElementById('timer-popup') &&
          !document.getElementById('timer-popup').hidden) {
        this.renderPopup();
      }
      this.updateCardIndicators();
      this.updateCardLive(); // KF-103: live card badge / revert on idle
    },

    // KF-001: the task the next session runs on. It survives stops, breaks
    // and reloads (a per-viewer convenience, so browser storage suffices).
    selectedTask: null,

    loadSelectedTask: function () {
      try {
        var raw = window.localStorage.getItem('chipflow.timerTask');
        var task = raw ? JSON.parse(raw) : null;
        return task && task.id ? task : null;
      } catch (e) {
        return null;
      }
    },

    // KF-001: drop a stored selection whose task is gone — checked on load
    // and whenever the selected card leaves the board (a delete; a move
    // re-adds it within the debounce).
    watchSelectedCard: function () {
      var self = this;
      var timer = null;
      var wasOnBoard = false;
      var check = function () {
        var task = self.selectedTask;
        var onBoard = !!task && !!document.querySelector('.task-card[data-task-id="' + cssEscape(String(task.id)) + '"]');
        if (task && !onBoard && (wasOnBoard || !self.selectionChecked)) self.validateSelection();
        self.selectionChecked = true;
        wasOnBoard = onBoard;
      };
      check();
      if (!window.MutationObserver) return;
      new MutationObserver(function () {
        window.clearTimeout(timer);
        timer = window.setTimeout(check, 300);
      }).observe(document.body, { childList: true, subtree: true });
    },

    // Resolves false (after dropping the selection) when the selected task
    // no longer exists, true otherwise.
    validateSelection: function () {
      var self = this;
      var task = this.selectedTask;
      if (!task) return Promise.resolve(true);
      return fetch('/api/tasks', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then(function (res) { return jsonIfJson(res); })
        .then(function (tasks) {
          if (!tasks || !self.selectedTask || String(self.selectedTask.id) !== String(task.id)) return true;
          var exists = tasks.some(function (t) { return String(t.id) === String(task.id); });
          if (!exists) self.dropSelection();
          return exists;
        })
        .catch(function () { return true; /* the next check retries */ });
    },

    dropSelection: function () {
      this.setSelectedTask(null);
      this.updateCardIndicators();
      var popup = document.getElementById('timer-popup');
      if (popup && !popup.hidden) this.renderPopup();
      this.renderPill();
    },

    setSelectedTask: function (task) {
      this.selectedTask = task && task.id ? task : null;
      try {
        if (this.selectedTask) {
          window.localStorage.setItem('chipflow.timerTask', JSON.stringify(this.selectedTask));
        } else {
          window.localStorage.removeItem('chipflow.timerTask');
        }
      } catch (e) { /* storage is optional */ }
    },

    // The running work session's task, else the selected task.
    currentTask: function () {
      var s = this.state;
      if (s && s.phase !== 'idle' && s.taskId) {
        return { id: String(s.taskId), name: s.taskName || '', color: s.taskColor };
      }
      return this.selectedTask;
    },

    // Seconds since the live session started, and seconds left of it
    // (negative once a timed session passes its duration).
    elapsed: function () {
      var s = this.state;
      if (!s || s.phase === 'idle') return 0;
      return Math.max(0, Math.floor(s.elapsedAtSync + (window.performance.now() - s.syncedAt) / 1000));
    },

    remaining: function () {
      var s = this.state;
      return ((s && s.totalSeconds) || 0) - this.elapsed();
    },

    // KF-197: the 1s tick used to start only inside renderPopup() and die in
    // closePopup(), so the header pill froze whenever the popup was closed
    // (updating only on the 15s server poll). The tick lifecycle now follows
    // the timer state instead: ticking while any session is live, stopped
    // when idle.
    syncTick: function () {
      var live = !!(this.state && this.state.phase !== 'idle');
      if (live && !this.tickHandle) {
        this.startTick();
      } else if (!live && this.tickHandle) {
        window.clearInterval(this.tickHandle);
        this.tickHandle = null;
      }
    },

    // KF-052: the timer's task gets a dashed outline on its card — the
    // selected task too while the timer is idle (KanbanFlow). KF-378: while
    // a work session runs or waits for its break, the board-header avatar
    // and the timed card's avatar carry a play badge; a timed card without
    // the user's avatar shows one for the session.
    updateCardIndicators: function () {
      var task = this.currentTask();
      var activeId = task ? String(task.id) : null;
      var s = this.state;
      var timing = !!(s && s.phase !== 'idle' && (s.mode === 'pomodoro' || s.mode === 'stopwatch'));
      var timedId = timing && s.taskId ? String(s.taskId) : null;
      var header = document.querySelector('.board-member-avatar');
      if (header) header.classList.toggle('avatar-timing', timing);
      var initials = header ? header.textContent.trim() : '';
      document.querySelectorAll('.task-card').forEach(function (card) {
        var selected = !!activeId && String(card.dataset.taskId) === activeId;
        card.classList.toggle('card-timer-selected', selected);
        var timed = !!timedId && String(card.dataset.taskId) === timedId;
        var added = card.querySelector('.card-avatar-timer');
        if (timed && !card.querySelector('.card-avatar')) {
          added = document.createElement('span');
          added.className = 'card-avatar card-avatar-timer';
          added.textContent = initials;
          var row = card.querySelector('.card-iconrow');
          var title = card.querySelector('.card-title');
          if (row) row.appendChild(added);
          else if (title) { title.appendChild(added); title.classList.add('card-title-has-avatar', 'card-title-timer-avatar'); }
        } else if (!timed && added) {
          var t = card.querySelector('.card-title-timer-avatar');
          if (t) t.classList.remove('card-title-has-avatar', 'card-title-timer-avatar');
          added.remove();
        }
        card.querySelectorAll('.card-avatar').forEach(function (a) {
          a.classList.toggle('avatar-timing', timed);
        });
      });
    },

    // KF-103: while a work session runs or waits for its break, the task's
    // card time reads "2h 46m + 42m / 8h" with the live part inline. A card
    // without a time badge gains one after the first minute. Stopping or
    // discarding drops the live part (hideCardLive).
    updateCardLive: function () {
      var s = this.state;
      var activeId = s && s.phase !== 'idle' && s.taskId &&
        (s.mode === 'pomodoro' || s.mode === 'stopwatch') ? String(s.taskId) : null;
      // KanbanFlow rounds to the nearest minute (1:35 reads "+ 2m", golden 25).
      var minutes = activeId ? Math.round(this.elapsed() / 60) : 0;
      var self = this;
      document.querySelectorAll('.task-card').forEach(function (card) {
        if (!activeId || String(card.dataset.taskId) !== activeId) {
          self.dropCardLive(card);
          return;
        }
        var live = card.querySelector('.card-live-inc');
        if (!live) live = self.addCardLive(card, minutes);
        if (live) live.textContent = ' + ' + minutes + 'm';
      });
    },

    // Insert the live span before the " / estimate" part of the badge text.
    // A task with no logged time gets a badge holding only the live part
    // ("⏱ + 2m", golden 25), once it reaches a minute.
    addCardLive: function (card, minutes) {
      var time = card.querySelector('.card-ico-time');
      if (!time) {
        if (minutes < 1) return null;
        time = document.createElement('span');
        time.className = 'card-ico-time card-live-made';
        time.innerHTML = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/></svg>';
        var row = card.querySelector('.card-iconrow');
        if (row) {
          row.insertBefore(time, row.querySelector('.card-avatar'));
        } else {
          row = document.createElement('div');
          row.className = 'card-iconrow card-live-made';
          row.appendChild(time);
          card.querySelector('.card-title').insertAdjacentElement('afterend', row);
        }
      }
      var live = document.createElement('span');
      live.className = 'card-live-inc';
      var text = time.lastChild;
      var slash = text && text.nodeType === 3 ? text.nodeValue.indexOf(' / ') : -1;
      if (slash >= 0) time.insertBefore(live, text.splitText(slash));
      else time.appendChild(live);
      return live;
    },

    dropCardLive: function (card) {
      var live = card.querySelector('.card-live-inc');
      if (!live) return;
      var time = live.parentNode;
      live.remove();
      time.normalize();
      var made = card.querySelector('.card-iconrow.card-live-made') || card.querySelector('.card-ico-time.card-live-made');
      if (made) made.remove();
    },

    hideCardLive: function () {
      var self = this;
      document.querySelectorAll('.task-card').forEach(function (card) { self.dropCardLive(card); });
    },

    fmt: function (seconds) {
      seconds = Math.max(0, Math.floor(seconds));
      var h = Math.floor(seconds / 3600);
      var m = Math.floor((seconds % 3600) / 60);
      var s = seconds % 60;
      var mm = (m < 10 ? '0' : '') + m;
      var ss = (s < 10 ? '0' : '') + s;
      return (h > 0 ? h + ':' + (m < 10 ? '0' + m : m) : mm) + ':' + ss;
    },

    pomodoroSeconds: function () {
      var mins = this.settings && this.settings.pomodoro_minutes ? this.settings.pomodoro_minutes : 25;
      return mins * 60;
    },

    // KF-098: idle pill shows the configured duration with a green play
    // triangle (pomodoro tab) or a red stop square + 00:00 (stopwatch tab);
    // a running session shows a red stop square + live countdown/count-up.
    renderPill: function () {
      var pill = document.getElementById('timer-pill');
      if (!pill) return;
      // KF-097: the pill is the timer dropdown control, but KanbanFlow
      // hides it while the timer popup panel is open. Update the
      // icon/label spans in place so the pill's own click listener
      // survives re-renders.
      var popup = document.getElementById('timer-popup');
      var popupOpen = !!(popup && !popup.hidden);
      pill.hidden = popupOpen;
      var s = this.state;
      var icon = document.getElementById('timer-pill-icon');
      var timeEl = document.getElementById('timer-pill-time');
      var active = !!(s && s.phase !== 'idle');
      var glyph = '&#9654;';
      var cls = 'timer-pill-icon timer-pill-play';
      var label;
      if (!active) {
        if (this.currentModeTab === 'stopwatch') {
          glyph = '&#9632;';
          cls = 'timer-pill-icon timer-pill-stop';
          label = '00:00';
        } else {
          label = this.fmt(this.pomodoroSeconds());
        }
      } else {
        // KF-325: a running session shows KanbanFlow's solid red square.
        glyph = '';
        cls = 'timer-pill-icon timer-pill-stopbox';
        label = this.fmt(s.mode === 'stopwatch' ? this.elapsed() : this.remaining());
      }
      // KF-020: the tab title counts down/up with the live timer
      // ("00:00 General — ChipFlow", KanbanFlow parity). KF-375: the title
      // keeps ticking while the popup hides the pill.
      if (!this._baseTitle) this._baseTitle = document.title;
      // An idle timer with a selected task shows its duration too (ref 03).
      document.title = active || this.selectedTask ? label + ' ' + this._baseTitle : this._baseTitle;
      if (icon) { icon.innerHTML = glyph; icon.className = cls; }
      if (timeEl) timeEl.textContent = label;
      // KF-027: the running state must be visibly distinct from idle.
      pill.classList.toggle('running', active);
    },

    // KF-001/KF-366: KanbanFlow's panel — a clock row (big time + Start,
    // Stop or Take break), the task row ("● Task ×" or "No task selected")
    // with its link, then the TODAY log.
    renderPopup: function () {
      var popup = document.getElementById('timer-popup');
      var body = document.getElementById('timer-popup-body');
      if (!popup || !body) return;
      var self = this;
      var s = this.state;
      var active = !!(s && s.phase !== 'idle');
      var mode = active ? s.mode : this.currentModeTab;
      var isBreak = mode === 'short_break' || mode === 'long_break';
      var titleEl = document.getElementById('timer-popup-title');
      var labelEl = document.getElementById('timer-popup-label');
      if (titleEl) {
        titleEl.textContent = mode === 'stopwatch' ? 'Stopwatch' :
          mode === 'long_break' ? 'Long break' : mode === 'short_break' ? 'Short break' : 'Pomodoro';
      }
      if (labelEl) {
        labelEl.textContent = mode === 'stopwatch' ? 'Session time' : isBreak ? 'Time left' : 'Time until break';
      }
      var clock;
      var button;
      if (this.waitingForBreak()) {
        // KF-006: a finished pomodoro keeps running (its time still counts)
        // until Take break; the clock gives way to the green button.
        clock = '';
        button = '<button type="button" class="timer-btn timer-btn-green" id="tp-take-break">Take break</button>';
      } else if (active) {
        clock = this.fmt(mode === 'stopwatch' ? this.elapsed() : this.remaining());
        button = '<button type="button" class="timer-btn" id="tp-stop">' +
          '<span class="timer-btn-stop" aria-hidden="true"></span>Stop</button>';
      } else {
        clock = mode === 'stopwatch' ? '00:00' : this.fmt(this.pomodoroSeconds());
        button = '<button type="button" class="timer-btn" id="tp-start">' +
          '<span class="timer-btn-play" aria-hidden="true"></span>Start</button>';
      }
      var html = '<div class="timer-clock-row"><span class="timer-clock">' + clock + '</span>' + button + '</div>';
      var task = this.currentTask();
      if (task) {
        html += '<div class="timer-task-row">' +
          '<span class="timer-task-dot" style="background:' + escapeHtml(task.color || '#9e9e9e') + '"></span>' +
          '<span class="timer-task-name">' + escapeHtml(task.name || '') + '</span>' +
          (active ? '' : '<button type="button" class="timer-task-clear" id="tp-clear-task" title="Clear task" aria-label="Clear task">&times;</button>') +
          '</div>';
      } else {
        html += '<div class="timer-task-row timer-task-none">No task selected</div>';
      }
      // KF-010: "Select open task" while a different task's modal is open.
      var openId = modalTaskId();
      var link = openId && (!task || String(openId) !== String(task.id)) ? 'Select open task' :
        task ? 'Change task' : 'Select task';
      html += '<div class="timer-task-link-row"><button type="button" class="timer-task-link" id="tp-change-task">' +
        link + '</button></div>';
      // KF-347: during a break the user can attribute the break time to a
      // defined break activity (KanbanFlow parity); the choice travels with
      // the stop request and feeds the statistics Break activities tab.
      var breakActs = (this.settings && this.settings.break_activities) || [];
      if (active && isBreak && breakActs.length) {
        html += '<div class="timer-break-activity"><label for="tp-break-activity">Break activity</label>' +
          '<select id="tp-break-activity"><option value="">No activity</option>' +
          breakActs.map(function (a) {
            return '<option value="' + escapeHtml(a.id) + '">' + escapeHtml(a.name) + '</option>';
          }).join('') + '</select></div>';
      }
      body.innerHTML = html;
      var on = function (id, fn) {
        var el = document.getElementById(id);
        if (el) el.addEventListener('click', fn);
      };
      on('tp-start', function () { self.startClicked(); });
      on('tp-stop', function () { self.stopClicked(); });
      on('tp-take-break', function () { self.takeBreak(); });
      on('tp-clear-task', function () { self.clearTask(); });
      on('tp-change-task', function () { self.changeTask(); });
      this.renderTodayList();
    },

    // KF-279: KanbanFlow's TODAY log — "TODAY 58m 1 Pomodoro" (the total
    // and count appear once something is logged), the live session first as
    // "10:35 PM – pending", then logged entries as "10:27 PM – 10:28 PM",
    // each with a task-colored dot and a ⋮ menu. Two rows show until "•••"
    // expands the rest.
    todayExpanded: false,

    renderTodayList: function () {
      var list = document.getElementById('timer-today-list');
      if (!list) return;
      var self = this;
      fetch('/api/timer/today', {
        headers: { 'Accept': 'application/json' },
        credentials: 'same-origin',
      }).then(function (res) { return jsonIfJson(res); })
        .then(function (entries) {
          self.drawToday((entries || []).filter(function (e) {
            return e.kind !== 'short_break' && e.kind !== 'long_break';
          }));
        })
        .catch(function () { /* today list is best-effort */ });
    },

    drawToday: function (entries) {
      var self = this;
      var list = document.getElementById('timer-today-list');
      var heading = document.getElementById('timer-today-heading');
      if (!list || !heading) return;
      var rows = [];
      var s = this.state;
      if (s && s.phase !== 'idle' && (s.mode === 'pomodoro' || s.mode === 'stopwatch')) {
        var task = this.currentTask();
        rows.push({
          pending: true,
          name: s.taskId && task ? task.name : 'No task',
          color: s.taskId && task ? task.color : null,
          when: clockTime(new Date(s.startedAt * 1000)) + '\u00a0 \u2013 \u00a0pending',
        });
      }
      entries.forEach(function (e) {
        rows.push({
          id: e.id,
          name: e.task_name,
          color: e.task_color,
          when: e.started_display + '\u00a0 \u2013 \u00a0' + e.ended_display,
        });
      });
      var completed = entries.filter(function (e) { return e.kind === 'pomodoro' && !e.interrupted; });
      this.todayCompleted = {
        count: completed.length,
        seconds: completed.reduce(function (sum, e) { return sum + (e.seconds || e.minutes * 60); }, 0),
      };
      this.todayRows = rows.length;
      this.drawTodayHeading();
      var shown = this.todayExpanded ? rows : rows.slice(0, 2);
      list.innerHTML = shown.map(function (r, i) {
        return '<div class="today-entry' + (r.pending ? ' today-pending' : '') + '">' +
          '<span class="today-dot" style="background:' + escapeHtml(r.color || '#9e9e9e') + '"></span>' +
          '<span class="today-text"><span class="today-task">' + escapeHtml(r.name || '') + '</span>' +
          '<span class="today-when">' + escapeHtml(r.when) + '</span></span>' +
          '<button type="button" class="today-kebab" data-today-row="' + i + '" title="Options" aria-label="Options" aria-haspopup="menu">' +
          '<svg width="4" height="12" viewBox="0 0 4 12" aria-hidden="true" focusable="false"><circle cx="2" cy="1.6" r="1.5"/><circle cx="2" cy="6" r="1.5"/><circle cx="2" cy="10.4" r="1.5"/></svg>' +
          '</button></div>';
      }).join('') + (rows.length > shown.length
        ? '<button type="button" class="today-more" title="Show all" aria-label="Show all entries">' +
          '<svg width="12" height="4" viewBox="0 0 12 4" aria-hidden="true" focusable="false"><circle cx="2" cy="2" r="1.6"/><circle cx="6" cy="2" r="1.6"/><circle cx="10" cy="2" r="1.6"/></svg></button>' : '');
      var more = list.querySelector('.today-more');
      if (more) more.addEventListener('click', function () { self.todayExpanded = true; self.drawToday(entries); });
      list.querySelectorAll('.today-kebab').forEach(function (btn) {
        btn.addEventListener('click', function (ev) {
          ev.stopPropagation();
          self.openTodayMenu(btn, shown[+btn.getAttribute('data-today-row')]);
        });
      });
    },

    // The heading total is completed pomodori plus the live pomodoro (also
    // while it waits for its break), rounded to the nearest minute and
    // hidden at 0: a bare "TODAY" for a 30 s stopped entry or a stopwatch
    // (refs 05, 07, goldens 23-24), "TODAY 2m" 1:35 into a pomodoro
    // (golden 25), "26m 1 Pomodoro" once logged (golden 27). The count is
    // logged pomodori only. Ticks with the clock.
    todayCompleted: { count: 0, seconds: 0 },
    todayRows: 0,

    drawTodayHeading: function () {
      var heading = document.getElementById('timer-today-heading');
      if (!heading) return;
      var s = this.state;
      var live = s && s.phase !== 'idle' && s.mode === 'pomodoro' ? this.elapsed() : 0;
      var minutes = Math.round((this.todayCompleted.seconds + live) / 60);
      var count = this.todayCompleted.count;
      var head = '<span class="timer-today-title">TODAY</span>';
      if (minutes > 0) head += '<span class="timer-today-total">' + fmtDuration(minutes) + '</span>';
      if (count) {
        head += '<span class="timer-today-count">' + count + (count === 1 ? ' Pomodoro' : ' Pomodoros') + '</span>';
      }
      if (heading.innerHTML !== head) heading.innerHTML = head;
      heading.hidden = !this.todayRows;
    },

    // The ⋮ menu: a logged entry can be edited or deleted (with a confirm);
    // the pending session can move to another task.
    openTodayMenu: function (btn, row) {
      var self = this;
      var menu = document.getElementById('timer-today-menu');
      if (!menu) {
        menu = document.createElement('div');
        menu.id = 'timer-today-menu';
        menu.className = 'menu-pop menu-floating timer-today-menu';
        menu.setAttribute('role', 'menu');
        document.body.appendChild(menu);
      }
      menu.innerHTML = row.pending
        ? '<button type="button" role="menuitem" data-today-act="change">Change task</button>'
        : '<button type="button" role="menuitem" data-today-act="edit">Edit entry</button>' +
          '<button type="button" role="menuitem" data-today-act="delete">Delete entry</button>';
      menu.onclick = function (e) {
        var item = e.target.closest('[data-today-act]');
        if (!item) return;
        hideFloatingMenus();
        var act = item.getAttribute('data-today-act');
        if (act === 'change') {
          self.beginSelect();
        } else if (act === 'edit') {
          EditEntry.open(row.id);
        } else {
          showConfirmDialog('Delete time entry', 'Delete this time entry?', 'Delete', function () {
            fetch('/api/time/entries/' + encodeURIComponent(row.id), {
              method: 'DELETE',
              credentials: 'same-origin',
            }).then(function () { self.renderTodayList(); })
              .catch(function () { toast('Could not delete the entry.'); });
          });
        }
      };
      var r = btn.getBoundingClientRect();
      placeMenu(menu, r.right - 4, r.bottom + 2);
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
      // KF-103: keep the running task's card badge live on every tick.
      this.updateCardLive();
      // KF-079: ticking sound per the Ticking mode setting.
      var mode = this.settings && this.settings.ticking_mode;
      if (mode === 'always') {
        this.tickSound();
      } else if (mode === 'timer_start') {
        this._tickCount = (this._tickCount || 0) + 1;
        if (this._tickCount <= 5) this.tickSound();
      }
      if (s.mode !== 'stopwatch') {
        // A stopwatch counts up and never completes on its own.
        s.remainingSeconds = this.remaining();
        if (s.remainingSeconds <= 0) {
          this.sessionReachedZero();
          if (s.mode !== 'pomodoro') return;
        }
      }
      this.renderPill();
      this.updatePip();
      this.drawTodayHeading();
      var clockEl = document.querySelector('#timer-popup .timer-clock');
      if (clockEl && !this.waitingForBreak()) {
        clockEl.textContent = this.fmt(s.mode === 'stopwatch' ? this.elapsed() : s.remainingSeconds);
      }
    },

    // ----- KF-078: Picture-in-Picture -----
    //
    // When the toggle is on and a session runs, open a small always-on-top
    // window (browser Picture-in-Picture) fed by a canvas showing the
    // countdown and the current task. It closes when the timer stops.

    pipOn: function () {
      return !!(this.settings && this.settings.pip_enabled) &&
        !!(document.pictureInPictureEnabled);
    },

    // Reconcile the PiP window with the current state: open/update while a
    // session is live, close when the timer is idle or PiP is disabled.
    syncPip: function () {
      var s = this.state;
      if (!s || s.phase === 'idle' || !this.pipOn()) {
        this.closePip();
        return;
      }
      if (!this.pipVideo) this.openPip();
      this.updatePip();
    },

    openPip: function () {
      var self = this;
      if (self.pipVideo || !self.pipOn()) return;
      try {
        var canvas = document.createElement('canvas');
        canvas.width = 320;
        canvas.height = 180;
        var video = document.createElement('video');
        video.muted = true;
        video.playsInline = true;
        video.style.cssText = 'position:fixed;left:-10000px;top:0;width:2px;height:2px;opacity:0;pointer-events:none;';
        document.body.appendChild(video);
        video.srcObject = canvas.captureStream(1);
        self.pipCanvas = canvas;
        self.pipVideo = video;
        video.addEventListener('loadedmetadata', function () {
          video.play().then(function () {
            if (!document.pictureInPictureElement && self.pipOn()) {
              video.requestPictureInPicture().catch(function () {
                // May need a user gesture; the next tick/sync retries.
              });
            }
          }).catch(function () { /* autoplay blocked; retry on sync */ });
        });
        self.updatePip();
      } catch (e) {
        self.closePip();
      }
    },

    updatePip: function () {
      if (!this.pipVideo || !this.pipCanvas) return;
      if (!this.pipOn()) { this.closePip(); return; }
      var s = this.state;
      var ctx = this.pipCanvas.getContext('2d');
      var w = this.pipCanvas.width, h = this.pipCanvas.height;
      ctx.fillStyle = '#111827';
      ctx.fillRect(0, 0, w, h);
      var time, label;
      if (!s || s.phase === 'idle') {
        time = '--:--';
        label = 'Timer stopped';
      } else if (s.mode === 'stopwatch') {
        time = this.fmt(this.elapsed());
        label = s.taskName || 'Stopwatch';
      } else {
        time = this.fmt(Math.max(0, s.remainingSeconds || 0));
        var modeLabel = { pomodoro: 'Pomodoro', short_break: 'Short break', long_break: 'Long break' }[s.mode] || 'Timer';
        label = s.taskName && s.mode === 'pomodoro' ? s.taskName : modeLabel;
      }
      ctx.fillStyle = '#f9fafb';
      ctx.textAlign = 'center';
      ctx.font = 'bold 64px system-ui, sans-serif';
      ctx.fillText(time, w / 2, h / 2 + 12);
      ctx.font = '20px system-ui, sans-serif';
      ctx.fillStyle = '#9ca3af';
      var short = label.length > 28 ? label.slice(0, 27) + '\u2026' : label;
      ctx.fillText(short, w / 2, h / 2 + 52);
    },

    closePip: function () {
      var self = this;
      try {
        if (document.pictureInPictureElement && self.pipVideo &&
            document.pictureInPictureElement === self.pipVideo) {
          document.exitPictureInPicture().catch(function () {});
        }
      } catch (e) { /* noop */ }
      if (self.pipVideo) {
        try {
          var stream = self.pipVideo.srcObject;
          if (stream && stream.getTracks) {
            stream.getTracks().forEach(function (t) { t.stop(); });
          }
          self.pipVideo.pause();
          if (self.pipVideo.parentNode) self.pipVideo.parentNode.removeChild(self.pipVideo);
        } catch (e) { /* noop */ }
        self.pipVideo = null;
        self.pipCanvas = null;
      }
    },

    togglePopup: function () {
      var popup = document.getElementById('timer-popup');
      if (!popup) return;
      if (popup.hidden) {
        popup.hidden = false;
        this.renderPopup();
        // KF-097: anchor after rendering so the height is known; renderPill
        // then hides the pill while the panel is open.
        this.positionPopup();
        this.renderPill();
      } else {
        this.closePopup();
      }
    },

    // KF-366: KanbanFlow drops the panel from the board-header row (6 px
    // below its top), right edge 15 px left of the filter funnel.
    positionPopup: function () {
      var popup = document.getElementById('timer-popup');
      if (!popup) return;
      var w = popup.offsetWidth || 260;
      var funnel = document.querySelector('#filter-btn svg');
      var row = document.querySelector('.board-header-row');
      var left;
      var top;
      if (funnel && row) {
        left = funnel.getBoundingClientRect().left - 15 - w;
        top = row.getBoundingClientRect().top + 6;
      } else {
        var pill = document.getElementById('timer-pill');
        var r = pill ? pill.getBoundingClientRect() : { left: window.innerWidth - w - 8, bottom: 40 };
        left = r.left;
        top = r.bottom + 6;
      }
      popup.style.left = Math.max(8, Math.min(left, window.innerWidth - w - 8)) + 'px';
      popup.style.top = top + 'px';
      popup.style.right = 'auto';
    },

    closePopup: function () {
      var popup = document.getElementById('timer-popup');
      if (popup) popup.hidden = true;
      // KF-097: the pill returns once the popup closes.
      var pill = document.getElementById('timer-pill');
      if (pill) pill.hidden = false;
      // KF-232: refresh the pill (icon, label, tab title) the popup hid.
      this.renderPill();
      // KF-197: keep the 1s tick alive while a session runs — the pill must
      // keep counting down with the popup closed. syncTick stops it on idle.
      this.syncTick();
    },

    // KF-282: actions for the timer-area right-click context menu (KanbanFlow
    // parity, GM-109). They act on the currently-timed task: Comment and
    // Label open the task modal and trigger the matching Add-menu item,
    // Edit opens the task modal, and Delete removes the task with the
    // standard in-page confirmation.
    timerMenuAction: function (act, taskId, taskName) {
      if (!taskId) return;
      var self = this;
      if (act === 'comment') {
        openModal(taskId);
        this.clickAfterModal('#tm-add-menu [data-tm-act="add-comment"]');
      } else if (act === 'label') {
        openModal(taskId);
        this.clickAfterModal('#tm-add-menu [data-tm-act="add-label"]');
      } else if (act === 'edit') {
        openModal(taskId);
      } else if (act === 'delete') {
        // Mirrors the card context menu's delete (KF-184): styled in-page
        // confirmation, card removal, and a column-count refresh.
        showConfirmDialog('Delete task', 'Delete "' + taskName + '"?', 'Delete', function () {
          api('/api/tasks/' + encodeURIComponent(taskId), 'DELETE')
            .then(function (res) {
              if (!res.ok) { toast('Could not delete task.'); return; }
              var card = document.querySelector('.task-card[data-task-id="' + cssEscape(taskId) + '"]');
              var list = card ? card.closest('.task-list') : null;
              var colId = list ? list.dataset.columnId : null;
              if (card && card.parentNode) card.parentNode.removeChild(card);
              if (colId) updateColumnCount(colId, -1);
              self.refresh();
            })
            .catch(function () { toast('Could not delete task.'); });
        });
      }
    },

    // The task modal loads its HTML asynchronously; click the given
    // in-modal button once it is wired (bounded poll, no-op on timeout).
    clickAfterModal: function (selector) {
      var tries = 0;
      var iv = window.setInterval(function () {
        var btn = document.querySelector('#modal-root ' + selector);
        tries++;
        if (btn || tries > 40) {
          window.clearInterval(iv);
          if (btn) btn.click();
        }
      }, 50);
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

    // KF-009: the footer's first tab switches the idle timer between
    // Pomodoro and Stopwatch and names the other mode.
    setModeTab: function (mode) {
      this.currentModeTab = mode;
      var foot = document.getElementById('timer-foot-mode');
      if (foot) {
        var other = mode === 'pomodoro' ? 'Stopwatch' : 'Pomodoro';
        foot.title = other;
        var span = foot.querySelector('span');
        if (span) span.textContent = other;
      }
      var popup = document.getElementById('timer-popup');
      if (popup && !popup.hidden) this.renderPopup();
      // The idle pill reflects the selected tab (KF-098: stopwatch idle).
      this.renderPill();
    },

    switchModeTab: function () {
      this.setModeTab(this.currentModeTab === 'pomodoro' ? 'stopwatch' : 'pomodoro');
    },

    // KF-001: the green Start runs the selected task, or no task.
    // KF-001: a selected task deleted elsewhere is dropped before Start
    // ("No task selected"), so Start never posts a stale id.
    startClicked: function () {
      var self = this;
      var mode = this.currentModeTab;
      this.validateSelection().then(function (exists) {
        if (!exists) return;
        var task = self.selectedTask;
        self.start(mode, task ? task.id : null);
      });
    },

    start: function (mode, taskId) {
      var self = this;
      if (taskId && (mode === 'pomodoro' || mode === 'stopwatch')) {
        this.setSelectedTask(this.taskInfo(taskId));
      }
      // Starting retires (and logs) a running work session.
      var retiredTask = this.workTaskId();
      fetch('/api/timer/start', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ mode: mode, task_id: taskId || null }),
      }).then(function (res) {
        if (res.ok) {
          self.refresh();
          self.closePopup();
          self.refreshTaskCard(retiredTask);
        } else if (res.status === 404 && taskId && self.selectedTask &&
                   String(self.selectedTask.id) === String(taskId)) {
          self.dropSelection(); // KF-001: the task was deleted meanwhile
        } else {
          res.text().then(function (t) { toast('Could not start timer: ' + t); });
        }
      });
    },

    startForTask: function (taskId) {
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
      if (this.elapsed() < 20) {
        this.stopAndLog(null);
        return;
      }
      this.beginWhy(s.sessionId, s.taskId, s.taskName, s.mode, s.remainingSeconds, s.totalSeconds);
    },

    // KF-006: a pomodoro past its duration waits for "Take break".
    waitingForBreak: function () {
      var s = this.state;
      return !!(s && s.phase !== 'idle' && s.mode === 'pomodoro' && this.remaining() <= 0);
    },

    // KF-006: at 00:00 a pomodoro chimes once and waits for "Take break"
    // (the server keeps it running, so a reload lands in the same state);
    // a break chimes and ends, leaving the selected task ready to Start.
    sessionReachedZero: function () {
      var self = this;
      var s = this.state;
      if (s.mode === 'pomodoro') {
        if (this.chimedFor === s.startedAt) return;
        this.chimedFor = s.startedAt;
        this.playChime();
        var popup = document.getElementById('timer-popup');
        if (popup && !popup.hidden) this.renderPopup();
        return;
      }
      // The server decides when a break is over: end it only once its
      // status agrees, otherwise resync and keep counting.
      if (this.endingBreak) return;
      this.endingBreak = true;
      fetch('/api/timer/status', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then(function (res) { return jsonIfJson(res); })
        .then(function (status) {
          if (!status) return null;
          var over = status.phase === s.mode && status.started_at === s.startedAt &&
            status.remaining_seconds === 0;
          if (!over) { self.updateFromStatus(status); return null; }
          return self.stopSession(true).then(function () { self.playChime(); });
        })
        .then(function () {
          self.endingBreak = false;
          self.refresh();
        }, function () { self.endingBreak = false; });
    },

    // Stop the live session. KF-347: a break carries the chosen break
    // activity; KF-015: a completed session carries no stop reason.
    stopSession: function (completed) {
      var payload = completed ? { completed: true } : {};
      var actSel = document.getElementById('tp-break-activity');
      if (actSel && actSel.value) payload.activity_id = actSel.value;
      return fetch('/api/timer/stop', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify(payload),
      }).catch(function () { /* the next status poll resyncs */ });
    },

    // KF-006: log the finished pomodoro, then start the due break. Only
    // completed pomodori count toward "Long break interval: Every Nth
    // break" (the server's pomodoro_count), so stopped ones never make a
    // break long.
    takeBreak: function () {
      var self = this;
      var every = this.settings && this.settings.long_break_every
        ? this.settings.long_break_every : 4;
      var loggedTask = this.workTaskId();
      this.stopSession(true)
        .then(function () {
          self.refreshTaskCard(loggedTask);
          return fetch('/api/timer/status', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' });
        })
        .then(function (res) { return res ? jsonIfJson(res) : null; })
        .then(function (status) {
          var count = status ? status.pomodoro_count : 0;
          self.startBreak(count > 0 && count % every === 0 ? 'long' : 'short');
        });
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
      this.whyPending = true; // KF-194: a stop is in flight until finalized
      var menu = document.getElementById('why-stop-menu');
      // The menu lists the configured interruption reasons; make sure the
      // settings payload arrived before rendering it.
      // KF-366: the menu takes the panel's place (KanbanFlow "‹ Why did you stop?").
      var popup = document.getElementById('timer-popup');
      if (popup && !popup.hidden && popup.style.left) {
        menu.style.left = popup.style.left;
        menu.style.top = popup.style.top;
        menu.style.right = 'auto';
      }
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
        .then(function (res) { return jsonIfJson(res); })
        .then(function (s) { if (s) self.settings = s; cb(); });
    },

    closeWhyMenu: function () {
      var menu = document.getElementById('why-stop-menu');
      if (menu) menu.hidden = true;
      // KF-194: dismissing the why menu without picking a reason (×/Esc)
      // must still finalize the stop — otherwise the server timer keeps
      // running while the UI looks idle (pill and popup disagree).
      if (this.whyPending) {
        this.whyPending = false;
        this.stopAndLog(null);
      }
    },

    stopAndLog: function (reason) {
      var self = this;
      var loggedTask = this.workTaskId();
      self.whyPending = false; // the stop is being finalized; a later
      // closeWhyMenu (e.g. from the fetch callback) must not re-trigger it.
      var payload = {};
      if (reason) payload.reason = reason;
      // KF-347: attribute a break session to the chosen break activity.
      var actSel = document.getElementById('tp-break-activity');
      if (actSel && actSel.value) payload.activity_id = actSel.value;
      fetch('/api/timer/stop', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify(payload),
      }).then(function (res) { return jsonIfJson(res); })
        .then(function (data) {
          self.closeWhyMenu();
          self.hideCardLive(); // KF-103: session over — badge reverts
          self.refresh();
          // KF-007: the server discards sessions under 20s — say so with
          // KanbanFlow's toast instead of pretending the stop was logged.
          if (data && data.discarded) {
            toast('Session discarded<span class="toast-sub">Session lasted less than 20 seconds</span>');
          } else if (data) {
            self.refreshTaskCard(loggedTask);
          }
        });
    },

    // The task of the live pomodoro or stopwatch session, if any.
    workTaskId: function () {
      var s = this.state;
      return s && s.phase !== 'idle' && s.taskId && (s.mode === 'pomodoro' || s.mode === 'stopwatch')
        ? String(s.taskId) : null;
    },

    // KF-103: once a session logs its time, the card shows the server's new
    // total (only a discard reverts to the old one).
    refreshTaskCard: function (taskId) {
      var self = this;
      if (!taskId) return;
      fetch('/api/tasks/' + encodeURIComponent(taskId) + '/card', { headers: { 'Accept': 'text/html' }, credentials: 'same-origin' })
        .then(function (res) { return res.ok ? res.text() : null; })
        .then(function (html) {
          var card = document.querySelector('.task-card[data-task-id="' + cssEscape(taskId) + '"]');
          if (!html || !card) return;
          var holder = document.createElement('div');
          holder.innerHTML = html;
          var fresh = holder.querySelector('.task-card');
          if (!fresh) return;
          card.replaceWith(fresh);
          applyCardSecState(fresh);
          self.updateCardIndicators();
          self.updateCardLive();
        })
        .catch(function () { /* the next board load shows the total */ });
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

    // KF-010: the task-row link. "Select open task" picks the task whose
    // modal is open; "Select task" and "Change task" enter select mode.
    changeTask: function () {
      var openId = modalTaskId();
      var task = this.currentTask();
      if (openId && (!task || String(openId) !== String(task.id))) {
        this.selectTask(openId);
        return;
      }
      this.beginSelect();
    },

    selecting: false,

    // KF-001: "Select task" closes the panel and shows the header pill; the
    // next card click picks that card (see init).
    beginSelect: function () {
      this.selecting = true;
      this.closePopup();
      var pill = document.getElementById('timer-select-pill');
      if (pill) pill.hidden = false;
    },

    endSelect: function () {
      this.selecting = false;
      var pill = document.getElementById('timer-select-pill');
      if (pill) pill.hidden = true;
    },

    // Point the timer at a task: a running work session moves to it, an idle
    // timer keeps it for the next Start. The panel reopens on the choice.
    selectTask: function (taskId) {
      if (!taskId) return;
      this.endSelect();
      // The picking click focused the card; drop its focus ring.
      if (document.activeElement && document.activeElement.blur) document.activeElement.blur();
      this.setSelectedTask(this.taskInfo(taskId));
      var s = this.state;
      if (s && s.phase !== 'idle' && (s.mode === 'pomodoro' || s.mode === 'stopwatch')) {
        this.retargetTimer(taskId);
      }
      this.updateCardIndicators();
      var popup = document.getElementById('timer-popup');
      if (popup && popup.hidden) this.togglePopup();
      else this.renderPopup();
    },

    clearTask: function () {
      this.setSelectedTask(null);
      this.updateCardIndicators();
      this.renderPopup();
    },

    // Name and dot color (the card's border color, as KanbanFlow's dot) of a
    // task on this board.
    taskInfo: function (taskId) {
      var card = document.querySelector('.task-card[data-task-id="' + cssEscape(String(taskId)) + '"]');
      var name = card ? card.dataset.taskName : '';
      var nameInput = document.getElementById('modal-name');
      if (!name && nameInput && String(modalTaskId()) === String(taskId)) name = nameInput.value;
      return {
        id: String(taskId),
        name: name || '',
        color: card ? window.getComputedStyle(card).borderTopColor : null,
      };
    },

    retargetTimer: function (taskId) {
      var self = this;
      fetch('/api/timer/retarget', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ task_id: taskId || null }),
      }).then(function (res) {
        // KF-129: only parse JSON when the server actually sent it.
        var ct = res.headers ? (res.headers.get('content-type') || '') : '';
        if (!res.ok || ct.indexOf('application/json') === -1) { toast('Could not change task.'); return null; }
        return res.json();
      }).then(function (status) {
        if (status) self.updateFromStatus(status);
        var popup = document.getElementById('timer-popup');
        if (popup && !popup.hidden) self.renderPopup();
      }).catch(function () { toast('Could not change task.'); });
    },

    addTime: function () {
      var id = modalTaskId();
      var nameInput = document.getElementById('modal-name');
      ManualTime.open(id, nameInput ? nameInput.value : null);
    },

    openLog: function () {
      // KF-286: the timer log opens as a modal over the dimmed board.
      TimerLogPage.openModal();
    },
  };

  // ---------- small shared helpers ----------

  var taskNameCache = {};

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

  // "5:43 PM", KanbanFlow's 12-hour clock.
  function clockTime(d) {
    return d.toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit' });
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

  // KF-004: seconds between "HH:MM[:SS]" strings; null when to <= from.
  function diffSeconds(from, to) {
    function parts(s) {
      var p = s.split(':');
      if (p.length < 2) return NaN;
      return (+p[0]) * 3600 + (+p[1]) * 60 + (p[2] ? +p[2] : 0);
    }
    var a = parts(from), b = parts(to);
    if (isNaN(a) || isNaN(b) || b <= a) return null;
    return b - a;
  }

  function fmtDuration(mins, duration_display) {
    // If duration_display is provided and not empty, use it
    if (duration_display && duration_display !== '') {
      return duration_display;
    }
    var h = Math.floor(mins / 60), m = mins % 60;
    if (h > 0 && m > 0) return h + 'h ' + m + 'm';
    if (h > 0) return h + 'h';
    return m + 'm';
  }

  // Shared calendar renderer for the manual-time and edit-entry dialogs:
  // Sun–Sat grid, selected day blue (KF-003).
  // KF-291: three-letter weekday names (KanbanFlow GM-122/GM-171) and quick
  // buttons "Today", "yesterday", "This month" under the grid.
  function renderCalPopup(popup, input, year, month) {
    var first = new Date(year, month, 1);
    var startOffset = first.getDay(); // 0 = Sunday
    var daysInMonth = new Date(year, month + 1, 0).getDate();
    var monthName = first.toLocaleString('en-US', { month: 'long', year: 'numeric' });
    var html = '<div class="mt-cal-head"><button type="button" data-cal-prev>&lt;</button>' +
      '<span>' + monthName + '</span><button type="button" data-cal-next>&gt;</button></div>' +
      '<div class="mt-cal-grid">';
    ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'].forEach(function (d) { html += '<span class="mt-cal-dow">' + d + '</span>'; });
    for (var i = 0; i < startOffset; i++) html += '<span></span>';
    for (var d = 1; d <= daysInMonth; d++) {
      var iso = year + '-' + String(month + 1).padStart(2, '0') + '-' + String(d).padStart(2, '0');
      var cls = 'mt-cal-day' + (iso === input.value ? ' selected' : '');
      html += '<button type="button" class="' + cls + '" data-date="' + iso + '">' + d + '</button>';
    }
    html += '</div>' +
      '<div class="mt-cal-quick">' +
      '<button type="button" data-cal-today>Today</button>' +
      '<button type="button" data-cal-yesterday>yesterday</button>' +
      '<button type="button" data-cal-thismonth>This month</button>' +
      '</div>';
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
    // KF-291 quick buttons. Today/yesterday pick the date (popups dismiss;
    // the inline due-date calendar re-renders so the picked day highlights).
    // "This month" only navigates the grid back to the current month.
    function setQuickDate(date) {
      input.value = isoDate(date);
      if (popup.classList.contains('mt-cal-popup')) {
        popup.hidden = true;
      } else {
        renderCalPopup(popup, input, date.getFullYear(), date.getMonth());
      }
    }
    popup.querySelector('[data-cal-today]').addEventListener('click', function (e) {
      e.stopPropagation();
      setQuickDate(new Date());
    });
    popup.querySelector('[data-cal-yesterday]').addEventListener('click', function (e) {
      e.stopPropagation();
      var y = new Date();
      y.setDate(y.getDate() - 1);
      setQuickDate(y);
    });
    popup.querySelector('[data-cal-thismonth]').addEventListener('click', function (e) {
      e.stopPropagation();
      var now = new Date();
      renderCalPopup(popup, input, now.getFullYear(), now.getMonth());
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
      var overlay = document.getElementById('manual-time-overlay');
      if (!overlay) return; // dialog not present on this page
      this.taskId = taskId || null;
      this.taskName = taskName || null;
      overlay.hidden = false;
      var taskInput = document.getElementById('mt-task');
      taskInput.value = taskName || '';
      document.getElementById('mt-date').value = date || isoDate(new Date());
      document.getElementById('mt-from').value = '';
      document.getElementById('mt-to').value = '';
      var comment = document.getElementById('mt-comment');
      comment.value = '';
      comment.hidden = true;
      var mtCommentCheck = document.getElementById('mt-comment-check');
      if (mtCommentCheck) mtCommentCheck.checked = false;
      ManualTimeLabels.reset();
      ManualTimeLabels.loadSuggestions();
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
    // KF-271: "Add comment" is a checkbox expander (GM-082); visibility
    // follows the checkbox state.
    toggleComment: function () {
      var check = document.getElementById('mt-comment-check');
      var ta = document.getElementById('mt-comment');
      ta.hidden = !(check && check.checked);
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
      // KF-380: catch a future start here; the server's 400 logs a console error.
      if (new Date(date + 'T' + (from.length === 5 ? from + ':00' : from)) > new Date()) {
        document.getElementById('mt-error-overlay').hidden = false;
        return;
      }
      fetch('/api/time/manual', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ task_id: taskId, date: date, from: from, to: to, note: note || null, labels: ManualTimeLabels.get() }),
      }).then(function (res) {
        if (res.ok) {
          self.close();
          if (modalTaskId()) { modalDirty = true; refreshModalLog(); }
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
        .then(function (res) { return jsonIfJson(res); })
        .then(function (data) {
          if (!data) { toast('Could not load the time entry.'); return; }
          self.taskId = data.task_id;
          self.taskName = data.task_name || '';
          var start = new Date(data.started_at);
          // KF-004: guard against malformed started_at (Invalid Date would
          // populate NaN into the date/time fields).
          if (isNaN(start.getTime())) { toast('Could not load the time entry.'); return; }
          document.getElementById('ee-date').value = isoDate(start);
          document.getElementById('ee-from').value = toTimeStr(start, true);
          var secs = data.seconds || (data.minutes || 0) * 60;
          document.getElementById('ee-to').value =
            toTimeStr(new Date(start.getTime() + secs * 1000), true);
          document.getElementById('ee-task').value = data.task_name || '';
          var comment = document.getElementById('ee-comment');
          comment.value = data.note || '';
          comment.hidden = !data.note;
          var eeCommentCheck = document.getElementById('ee-comment-check');
          if (eeCommentCheck) eeCommentCheck.checked = !!data.note;
          EditEntryLabels.reset();
          EditEntryLabels.loadSuggestions();
          EditEntryLabels.set(data.labels || []);
          self.hideError();
          self.updateDuration();
          ManualTime.tasksPromise = ManualTime.loadTaskList();
          document.getElementById('edit-entry-overlay').hidden = false;
        })
        .catch(function () { toast('Could not load the time entry.'); });
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
      var secs = diffSeconds(from, to);
      el.textContent = secs === null ? '—'
        : secs < 60 ? secs + 's' : fmtDuration(Math.floor(secs / 60));
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
    // KF-271: "Add comment" is a checkbox expander (GM-084); visibility
    // follows the checkbox state.
    toggleComment: function () {
      var check = document.getElementById('ee-comment-check');
      var ta = document.getElementById('ee-comment');
      ta.hidden = !(check && check.checked);
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
      if (diffSeconds(from, to) === null) { this.showError('End time must be after start time.'); return; }
      var btn = document.getElementById('ee-update');
      if (btn) { btn.disabled = true; btn.textContent = 'Updating…'; }
      fetch('/api/time/entries/' + encodeURIComponent(this.entryId), {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ task_id: taskId, date: date, from: from, to: to, note: note || null, labels: EditEntryLabels.get() }),
      }).then(function (res) {
        if (btn) { btn.disabled = false; btn.textContent = 'Update'; }
        if (res.ok) {
          self.close();
          var logOverlay = document.getElementById('timer-log-overlay');
          if (modalTaskId()) { modalDirty = true; refreshModalLog(); }
          else if (logOverlay && !logOverlay.hidden) document.getElementById('log-reload').click();
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
      // KF-277: the time-log entry row itself carries data-entry-id (the
      // explicit Edit button is gone), so ignore clicks on the row's
      // delete button — the delete handler is registered later on
      // document and cannot pre-empt this one via stopPropagation.
      if (e.target.closest('[data-delete-entry]')) return;
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
    var mtCommentCheck = document.getElementById('mt-comment-check');
    if (mtCommentCheck) mtCommentCheck.addEventListener('change', function () { ManualTime.toggleComment(); });
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
    var eeCommentCheck = document.getElementById('ee-comment-check');
    if (eeCommentCheck) eeCommentCheck.addEventListener('change', function () { EditEntry.toggleComment(); });
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
      if (modal) {
        var timerBtn = document.querySelector('[data-tm-menu="tm-timer-menu"]');
        if (timerBtn) timerBtn.click();
      } else {
        TimerUI.togglePopup();
      }
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

  // ---------- Task extras: labels, due dates, comments, attachments,
  // history & time-log sub-views (KanbanFlow parity batch) ----------

  // Labels dialog (KF-061): current labels with × removal, an add input,
  // and "Add labels..." suggestions drawn from the board's existing
  // labels (GET /api/boards/:id/labels). Save persists via PATCH.
  var LabelsDialog = {
    taskId: null,
    labels: [],
    suggestions: [],
    open: function () {
      var id = modalTaskId();
      if (!id) return;
      this.taskId = id;
      var self = this;
      var detailP = api('/api/tasks/' + encodeURIComponent(id), 'GET')
        .then(function (res) { return res.ok ? res.json() : Promise.reject(); });
      var suggP = api('/api/boards/' + encodeURIComponent(boardId()) + '/labels', 'GET')
        .then(function (res) { return res.ok ? res.json() : []; })
        .catch(function () { return []; });
      Promise.all([detailP, suggP]).then(function (results) {
        self.labels = (results[0].labels || []).slice();
        self.suggestions = results[1] || [];
        self.render();
        document.getElementById('labels-input').value = '';
        document.getElementById('labels-dialog').hidden = false;
        document.getElementById('labels-input').focus();
      }).catch(function () { toast('Could not load labels.'); });
    },
    render: function () {
      var self = this;
      var cur = document.getElementById('labels-current');
      cur.innerHTML = '';
      this.labels.forEach(function (l) {
        var chip = document.createElement('span');
        chip.className = 'tm-label-chip';
        chip.textContent = l + ' ';
        var x = document.createElement('button');
        x.type = 'button';
        x.className = 'tm-label-x';
        x.textContent = '×';
        x.setAttribute('aria-label', 'Remove label ' + l);
        x.addEventListener('click', function () { self.remove(l); });
        chip.appendChild(x);
        cur.appendChild(chip);
      });
      // KF-269: KanbanFlow's fully-empty labels dialog shows a single body
      // text "No recently used labels exist" (GM-080, GM-158) — there is no
      // separate "current labels" empty note, so render chips only when
      // labels exist and nothing otherwise.
      var sug = document.getElementById('labels-suggestions');
      sug.innerHTML = '';
      this.suggestions
        .filter(function (s) { return self.labels.indexOf(s) === -1; })
        .forEach(function (s) {
          var b = document.createElement('button');
          b.type = 'button';
          b.className = 'tm-label-chip tm-label-sug';
          b.textContent = s;
          b.setAttribute('aria-label', 'Add label ' + s);
          b.addEventListener('click', function () { self.add(s); });
          sug.appendChild(b);
        });
      // KF-269: KanbanFlow's suggestions-area empty text is verbatim
      // "No recently used labels exist" (GM-080, GM-158).
      if (!sug.children.length) {
        sug.innerHTML = '<span class="empty-note">No recently used labels exist</span>';
      }
    },
    add: function (label) {
      label = (label || '').trim();
      if (!label || this.labels.indexOf(label) !== -1) return;
      this.labels.push(label);
      this.render();
      // KF-328: KanbanFlow applies label changes immediately (GM-080 has
      // no Save button) — persist on every add/remove.
      this.persist();
    },
    remove: function (label) {
      this.labels = this.labels.filter(function (l) { return l !== label; });
      this.render();
      this.persist();
    },
    // PATCH the current label set; the dialog stays open.
    persist: function () {
      var self = this;
      // KF-157: PATCH /api/tasks/:id returns the refreshed task card as an
      // HTML fragment, not JSON — parse as text so a successful save is not
      // misreported as a failure.
      api('/api/tasks/' + encodeURIComponent(this.taskId), 'PATCH', { labels: this.labels })
        .then(function (res) { if (!res.ok) throw new Error('save failed'); return res.text(); })
        .catch(function () { toast('Could not save labels.'); });
    },
    close: function () {
      document.getElementById('labels-dialog').hidden = true;
      // Refresh the underlying modal so its label chips reflect the
      // immediately-persisted set.
      if (this.taskId) openModal(this.taskId, true);
    },
  };

  // Due-date dialog (KF-270 — KanbanFlow parity "Add due date": title,
  // Date/Time fields, inline calendar grid, runtime column list with
  // selection, Repeat field, green Add button).
  var DueDateDialog = {
    taskId: null,
    open: function () {
      var id = modalTaskId();
      if (!id) return;
      this.taskId = id;
      var self = this;
      api('/api/tasks/' + encodeURIComponent(id), 'GET')
        .then(function (res) { return res.ok ? res.json() : Promise.reject(); })
        .then(function (detail) {
          var dateInput = document.getElementById('dd-date');
          var timeInput = document.getElementById('dd-time');
          dateInput.value = '';
          timeInput.value = '';
          if (detail.due_at) {
            var d = new Date(detail.due_at);
            if (!isNaN(d.getTime())) {
              dateInput.value = isoDate(d);
              timeInput.value = toTimeStr(d);
            }
          }
          // KF-328: KanbanFlow's repeat is a checkbox (GM-081), not free text.
          document.getElementById('dd-repeat').checked = !!detail.due_repeat;
          self.renderColumnList(id);
          self.renderCalendar();
          document.getElementById('duedate-dialog').hidden = false;
        })
        .catch(function () { toast('Could not load the due date.'); });
    },
    // Inline calendar grid: same markup as renderCalPopup, but day clicks
    // stay inline (capture-phase stopPropagation blocks the popup's own
    // "hide on pick" listener; see the one-time wiring below).
    renderCalendar: function () {
      var input = document.getElementById('dd-date');
      var box = document.getElementById('dd-cal-inline');
      if (!box || !input) return;
      var current = input.value ? new Date(input.value + 'T12:00:00') : new Date();
      renderCalPopup(box, input, current.getFullYear(), current.getMonth());
    },
    // Runtime columns (never seeded names), with the task's own column
    // pre-selected. KF-328: a <select> dropdown like KanbanFlow (GM-081).
    renderColumnList: function (id) {
      var sel = document.getElementById('dd-column');
      sel.innerHTML = '';
      var card = document.querySelector('.task-card[data-task-id="' + cssEscape(id) + '"]');
      var ownList = card ? card.closest('.task-list') : null;
      var ownCol = ownList ? ownList.getAttribute('data-column-id') : null;
      var ths = document.querySelectorAll('.columnHeader[data-column-id]');
      Array.prototype.forEach.call(ths, function (th) {
        var opt = document.createElement('option');
        opt.value = th.getAttribute('data-column-id');
        opt.textContent = th.dataset.columnName || 'Column';
        if (opt.value === ownCol) opt.selected = true;
        sel.appendChild(opt);
      });
      if (!ths.length) {
        var empty = document.createElement('option');
        empty.textContent = 'No columns.';
        sel.appendChild(empty);
      }
    },
    // { due_at, due_repeat } for the dialog fields; null when the
    // date/time combination is invalid.
    collect: function () {
      var date = document.getElementById('dd-date').value;
      var time = document.getElementById('dd-time').value || '09:00';
      // KF-328: KanbanFlow's repeat is a checkbox (GM-081).
      var repeat = document.getElementById('dd-repeat').checked ? 'every week' : null;
      if (!date) return { due_at: null, due_repeat: repeat };
      var d = new Date(date + 'T' + (time.length === 5 ? time + ':00' : time));
      if (isNaN(d.getTime())) return null;
      return { due_at: d.toISOString(), due_repeat: repeat };
    },
    // Writes only the open task; the column select never picks which
    // tasks are written (the API has no due target-column field).
    save: function (patch, verb) {
      var self = this;
      if (!this.taskId) return;
      api('/api/tasks/' + encodeURIComponent(this.taskId), 'PATCH', patch).then(function (res) {
        if (!res.ok) throw new Error('save failed');
      }).then(function () {
        // KF-225: the dialog save must flag the board dirty synchronously —
        // openModal's refresh sets modalDirty only after its fetch resolves,
        // which left the board card stale when the fetch was slow or failed.
        modalDirty = true;
        document.getElementById('duedate-dialog').hidden = true;
        openModal(self.taskId, true);
        toast(verb + '.');
      }).catch(function () { toast('Could not save the due date.'); });
    },
    add: function () {
      var patch = this.collect();
      if (!patch) { toast('Invalid date or time.'); return; }
      this.save(patch, patch.due_at ? 'Due date added' : 'Due date cleared');
    },
  };
  // KF-270: capture-phase day clicks on the inline calendar keep the grid
  // visible (stopPropagation blocks renderCalPopup's hide-on-pick), then
  // re-render so the picked day is highlighted.
  (function () {
    var box = document.getElementById('dd-cal-inline');
    if (!box) return;
    box.addEventListener('click', function (e) {
      var btn = e.target.closest ? e.target.closest('[data-date]') : null;
      if (!btn) return;
      e.stopPropagation();
      var input = document.getElementById('dd-date');
      input.value = btn.getAttribute('data-date');
      var current = new Date(input.value + 'T12:00:00');
      renderCalPopup(box, input, current.getFullYear(), current.getMonth());
    }, true);
  })();

  // Comments (KF-064): the modal body always shows the section; Add →
  // Comment focuses the input.
  function addModalComment() {
    var id = modalTaskId();
    var ta = document.getElementById('modal-comment-input');
    if (!id || !ta) return;
    var body = ta.value.trim();
    if (!body) { ta.focus(); return; }
    api('/api/tasks/' + encodeURIComponent(id) + '/comments', 'POST', { body: body })
      .then(function (res) { if (!res.ok) throw new Error('add failed'); return res.json(); })
      .then(function () { refreshModal(); toast('Comment added.'); })
      .catch(function () { toast('Could not add the comment.'); });
  }

  function deleteModalComment(commentId) {
    var id = modalTaskId();
    if (!id || !commentId) return;
    // KF-188: styled in-page confirmation (same class as KF-184); the native
    // window.confirm() auto-dismisses under automation, silently no-op'ing.
    showConfirmDialog('Delete comment', 'Delete this comment?', 'Delete', function () {
      api('/api/tasks/' + encodeURIComponent(id) + '/comments/' + encodeURIComponent(commentId), 'DELETE')
        .then(function (res) { if (!res.ok) throw new Error('delete failed'); refreshModal(); })
        .catch(function () { toast('Could not delete the comment.'); });
    });
  }

  // KF-254: emoji picker for the per-comment smiley tool. KanbanFlow opens
  // a reaction picker; ChipFlow has no comment reactions, so picking an
  // emoji inserts it into the comment composer instead.
  var EMOJI_SET = ['\u{1F600}', '\u{1F601}', '\u{1F602}', '\u{1F642}', '\u{1F609}', '\u{1F60D}', '\u{1F44D}', '\u{1F44E}', '\u{1F389}', '\u2764\uFE0F', '\u{1F680}', '\u{1F440}'];
  var emojiPop = null;
  var emojiAnchor = null;

  function closeEmojiPop() {
    if (emojiPop) emojiPop.hidden = true;
    emojiAnchor = null;
  }

  function toggleEmojiPop(btn) {
    if (emojiPop && !emojiPop.hidden && emojiAnchor === btn) { closeEmojiPop(); return; }
    closeAllTmMenus();
    if (!emojiPop) {
      emojiPop = document.createElement('div');
      emojiPop.className = 'tm-emoji-pop';
      emojiPop.hidden = true;
      emojiPop.setAttribute('role', 'menu');
      emojiPop.setAttribute('aria-label', 'Insert emoji');
      EMOJI_SET.forEach(function (ch) {
        var b = document.createElement('button');
        b.type = 'button';
        b.setAttribute('data-emoji', ch);
        b.textContent = ch;
        b.setAttribute('aria-label', 'Insert ' + ch);
        emojiPop.appendChild(b);
      });
      document.body.appendChild(emojiPop);
    }
    emojiAnchor = btn;
    emojiPop.hidden = false;
    var r = btn.getBoundingClientRect();
    var pw = emojiPop.offsetWidth;
    var ph = emojiPop.offsetHeight;
    var left = Math.max(8, Math.min(r.right - pw, window.innerWidth - pw - 8));
    var top = r.bottom + 4;
    if (top + ph > window.innerHeight - 8) top = Math.max(8, r.top - ph - 4);
    emojiPop.style.left = left + 'px';
    emojiPop.style.top = top + 'px';
  }

  function insertEmoji(ch) {
    closeEmojiPop();
    var input = document.getElementById('modal-comment-input');
    if (!input || !ch) return;
    var start = input.selectionStart == null ? input.value.length : input.selectionStart;
    var end = input.selectionEnd == null ? start : input.selectionEnd;
    input.value = input.value.slice(0, start) + ch + input.value.slice(end);
    var pos = start + ch.length;
    input.focus();
    try { input.setSelectionRange(pos, pos); } catch (ignore) {}
  }

  // (KF-332: the modal Attachments section was removed; the upload/delete
  // API routes remain for programmatic use.)

  function deleteTimeEntry(entryId) {
    if (!entryId) return;
    // KF-188: styled in-page confirmation (same class as KF-184).
    showConfirmDialog('Delete time entry', 'Delete this time entry?', 'Delete', function () {
      api('/api/time/entries/' + encodeURIComponent(entryId), 'DELETE')
        .then(function (res) {
        if (!res.ok) throw new Error('delete failed');
        modalDirty = true;
        refreshModalLog();
      })
      .catch(function () { toast('Could not delete the entry.'); });
    });
  }

  // Labels chip editor shared by the manual-time and edit-entry dialogs
  // (KanbanFlow parity, KF-102).
  function makeLabelEditor(prefix) {
    var labels = [];
    function listEl() { return document.getElementById(prefix + '-labels-list'); }
    function wrapEl() { return document.getElementById(prefix + '-labels'); }
    function toggleEl() { return document.getElementById(prefix + '-labels-check'); }
    function inputEl() { return document.getElementById(prefix + '-labels-input'); }
    function render() {
      var list = listEl();
      if (!list) return;
      list.innerHTML = '';
      labels.forEach(function (l) {
        var chip = document.createElement('span');
        chip.className = 'tm-label-chip';
        chip.textContent = l + ' ';
        var x = document.createElement('button');
        x.type = 'button';
        x.className = 'tm-label-x';
        x.textContent = '×';
        x.setAttribute('aria-label', 'Remove label ' + l);
        x.addEventListener('click', function () {
          labels = labels.filter(function (v) { return v !== l; });
          render();
        });
        chip.appendChild(x);
        list.appendChild(chip);
      });
    }
    return {
      get: function () { return labels.slice(); },
      set: function (next) {
        labels = (next || []).slice();
        render();
        var wrap = wrapEl();
        if (wrap && labels.length) {
          wrap.hidden = false;
          var t = toggleEl();
          if (t) t.checked = true;
        }
      },
      reset: function () {
        labels = [];
        render();
        var wrap = wrapEl();
        if (wrap) wrap.hidden = true;
        var t = toggleEl();
        if (t) t.checked = false;
        var input = inputEl();
        if (input) input.value = '';
      },
      // KF-271: "Add labels" is a checkbox expander (GM-082/GM-084); the
      // section visibility follows the checkbox state.
      toggle: function () {
        var wrap = wrapEl();
        var t = toggleEl();
        if (!wrap || !t) return;
        wrap.hidden = !t.checked;
        if (!wrap.hidden) {
          var input = inputEl();
          if (input) input.focus();
        }
      },
      addFromInput: function () {
        var input = inputEl();
        if (!input) return;
        var v = input.value.trim();
        input.value = '';
        if (!v || labels.indexOf(v) !== -1) return;
        labels.push(v);
        render();
      },
      loadSuggestions: function () {
        var dl = document.getElementById(prefix + '-labels-datalist');
        var bid = boardId();
        if (!dl || !bid) return;
        api('/api/boards/' + encodeURIComponent(bid) + '/labels', 'GET')
          .then(function (res) { return res.ok ? res.json() : []; })
          .then(function (all) {
            dl.innerHTML = '';
            (all || []).forEach(function (s) {
              var opt = document.createElement('option');
              opt.value = s;
              dl.appendChild(opt);
            });
          })
          .catch(function () { /* suggestions are best-effort */ });
      },
    };
  }
  var ManualTimeLabels = makeLabelEditor('mt');
  var EditEntryLabels = makeLabelEditor('ee');

  // One-time wiring for the task-extras surfaces (same defer double-fire
  // guard pattern as initEntryEdit).
  var taskExtrasInitialized = false;
  function initTaskExtras() {
    if (taskExtrasInitialized) return;
    taskExtrasInitialized = true;

    // Dedicated sub-view navigation (back / + ADD ENTRY).
    document.addEventListener('click', function (e) {
      var sv = e.target.closest('[data-tm-subview]');
      if (!sv) return;
      var kind = sv.getAttribute('data-tm-subview');
      var id = modalTaskId();
      if (kind === 'back') {
        if (id) openModal(id);
      } else if (kind === 'add-entry') {
        var nameInput = document.getElementById('modal-name');
        ManualTime.open(id, nameInput ? nameInput.value : '');
      }
    });

    // Red trash icon on time-log sub-view entries.
    document.addEventListener('click', function (e) {
      var del = e.target.closest('[data-delete-entry]');
      if (!del) return;
      e.preventDefault();
      e.stopPropagation();
      deleteTimeEntry(del.getAttribute('data-delete-entry'));
    });

    // Modal comments (KF-254): the per-comment ⋮ menu carries Delete; the
    // smiley opens an emoji picker that inserts into the composer input.
    document.addEventListener('click', function (e) {
      var emojiBtn = e.target.closest('[data-comment-emoji]');
      if (emojiBtn) { toggleEmojiPop(emojiBtn); return; }
      var emojiPick = e.target.closest('.tm-emoji-pop button');
      if (emojiPick) { insertEmoji(emojiPick.getAttribute('data-emoji')); return; }
      if (!e.target.closest('.tm-emoji-pop')) closeEmojiPop();
      var cdel = e.target.closest('[data-comment-delete]');
      if (cdel) { deleteModalComment(cdel.getAttribute('data-comment-delete')); }
    });

    // Labels dialog.
    var labelsInput = document.getElementById('labels-input');
    if (labelsInput) labelsInput.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') { e.preventDefault(); LabelsDialog.add(labelsInput.value); labelsInput.value = ''; }
    });
    // KF-328: the labels × closes via LabelsDialog.close() so the modal
    // refreshes after immediate-persist edits.
    var labelsDialog = document.getElementById('labels-dialog');
    if (labelsDialog) labelsDialog.addEventListener('click', function (e) {
      if (e.target.closest('[data-close-dialog]')) LabelsDialog.close();
    });

    // Due-date dialog (KF-270: "Add due date" with inline calendar).
    var ddAdd = document.getElementById('dd-add');
    if (ddAdd) ddAdd.addEventListener('click', function () { DueDateDialog.add(); });

    // KF-253: description editor dialog.
    var descSave = document.getElementById('desc-editor-save');
    if (descSave) descSave.addEventListener('click', saveDescriptionEditor);
    var descInput = document.getElementById('desc-editor-input');
    if (descInput) descInput.addEventListener('keydown', function (e) {
      // Ctrl/Cmd+Enter saves from the editor. stopPropagation keeps the
      // document-level keydown handler from also blurring + closing the
      // task modal (Escape is left to the unified handleEscape, which
      // closes the topmost .dlg-overlay).
      if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
        e.preventDefault(); e.stopPropagation(); saveDescriptionEditor();
      }
    });

    // Labels editors in the manual-time / edit-entry dialogs (KF-102);
    // KF-271: the toggles are "Add labels" checkbox expanders.
    var mtLabelsCheck = document.getElementById('mt-labels-check');
    if (mtLabelsCheck) mtLabelsCheck.addEventListener('change', function () { ManualTimeLabels.toggle(); });
    var mtInput = document.getElementById('mt-labels-input');
    if (mtInput) mtInput.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') { e.preventDefault(); ManualTimeLabels.addFromInput(); }
    });
    var eeLabelsCheck = document.getElementById('ee-labels-check');
    if (eeLabelsCheck) eeLabelsCheck.addEventListener('change', function () { EditEntryLabels.toggle(); });
    var eeInput = document.getElementById('ee-labels-input');
    if (eeInput) eeInput.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') { e.preventDefault(); EditEntryLabels.addFromInput(); }
    });
  }

  // Unified Escape: submenus, floating menus, dialogs, time dialogs, why
  // menu, then the task modal — innermost surface first.
  function handleEscape() {
    if (typeof closeEmojiPop === 'function' && emojiPop && !emojiPop.hidden) { closeEmojiPop(); return; }
    if (closeAllTmMenus()) return;
    if (hideFloatingMenus()) return;
    // KF-001: Escape leaves the timer's "Select task" mode.
    if (TimerUI.selecting) { TimerUI.endSelect(); return; }
    // KF-328: the labels dialog refreshes the modal on close.
    var labelsDlg = document.getElementById('labels-dialog');
    if (labelsDlg && !labelsDlg.hidden && typeof LabelsDialog !== 'undefined') {
      LabelsDialog.close();
      return;
    }
    // Topmost overlays (KF-003: the manual-time future-time error) dismiss
    // before the dialog beneath them — innermost surface first, so Escape
    // preserves the dialog state instead of closing it out from under the
    // error.
    var top = document.querySelector('.dlg-overlay-top:not([hidden])');
    if (top) { top.hidden = true; return; }
    var open = document.querySelector('.dlg-overlay:not([hidden])');
    if (open) { open.hidden = true; return; }
    var mt = document.getElementById('manual-time-overlay');
    if (mt && !mt.hidden) { ManualTime.close(); return; }
    var ee = document.getElementById('edit-entry-overlay');
    if (ee && !ee.hidden) { EditEntry.close(); return; }
    var why = document.getElementById('why-stop-menu');
    if (why && !why.hidden) { TimerUI.closeWhyMenu(); return; }
    // KF-299: two-stage Escape for the task modal — first Esc discards
    // in-progress edits while keeping the modal open; a second Esc with
    // nothing pending closes the modal.
    if (discardModalDrafts()) return;
    closeModal();
  }

  // KF-299: returns true (and clears the drafts) when the open task modal
  // holds unsent edits — the comment composer or the subtask input. The
  // name field commits on change and the description edits through its own
  // dialog, so neither has a discardable draft here.
  function discardModalDrafts() {
    var modal = document.querySelector('#modal-root .task-modal');
    if (!modal) return false;
    var discarded = false;
    var commentTa = document.getElementById('modal-comment-input');
    if (commentTa && commentTa.value.trim() !== '') { commentTa.value = ''; discarded = true; }
    var subInput = document.getElementById('modal-subtask-input');
    if (subInput && subInput.value.trim() !== '') { subInput.value = ''; discarded = true; }
    return discarded;
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
    var boardsLoaded = false;

    function qs(id) { return document.getElementById(id); }

    // ---------- period stepper (KF-286) ----------
    // KanbanFlow renders the period as a stepper ("Period: < This month >")
    // with prev/next arrows instead of a plain <select>.
    var PERIODS = [
      { value: 'this-plus-last-week', label: 'This + Last week' },
      { value: 'this-week', label: 'This week' },
      { value: 'last-week', label: 'Last week' },
      { value: 'this-month', label: 'This month' },
      { value: 'last-month', label: 'Last month' },
      { value: 'custom-absolute', label: 'Custom (absolute)' },
      { value: 'custom-relative', label: 'Custom (relative)' },
    ];
    var periodIdx = 3; // default "This month" (GM-117)

    function currentPeriod() { return PERIODS[periodIdx]; }

    function renderPeriodLabel() {
      var el = qs('log-period-label');
      if (el) el.textContent = currentPeriod().label;
      var rel = qs('log-custom-relative');
      if (rel) rel.hidden = currentPeriod().value !== 'custom-relative';
    }

    function stepPeriod(dir) {
      periodIdx = (periodIdx + dir + PERIODS.length) % PERIODS.length;
      renderPeriodLabel();
      // KF-290: stepping onto Custom (absolute) opens the two-calendar
      // dialog; the log reloads only when the user confirms with Done.
      if (currentPeriod().value === 'custom-absolute') {
        CustomRange.open();
        return;
      }
      if (currentPeriod().value !== 'custom-relative') {
        loadLog();
      }
    }

    // ---------- custom absolute range dialog (KF-290) ----------
    // KanbanFlow (GM-121): nested "Custom (absolute)" dialog with Start/End
    // fields, two side-by-side month calendars with the selected dates
    // highlighted, and a green Done button. The chosen range is stored in the
    // hidden log-custom-from/to inputs that currentFilters() already reads.
    var CustomRange = (function () {
      var DOW = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
      var startISO = '', endISO = '';
      var active = 'start'; // which field a calendar day-click fills
      var left = { y: 0, m: 0 }, right = { y: 0, m: 0 };

      function toISO(d) {
        return d.getFullYear() + '-' + String(d.getMonth() + 1).padStart(2, '0') +
          '-' + String(d.getDate()).padStart(2, '0');
      }
      function parseISO(s) {
        var m = /^(\d{4})-(\d{2})-(\d{2})$/.exec((s || '').trim());
        if (!m) return null;
        var d = new Date(+m[1], +m[2] - 1, +m[3]);
        return (d.getFullYear() === +m[1] && d.getMonth() === +m[2] - 1 &&
          d.getDate() === +m[3]) ? d : null;
      }
      function shift(ym, dir) {
        var m = ym.m + dir, y = ym.y;
        while (m < 0) { m += 12; y--; }
        while (m > 11) { m -= 12; y++; }
        ym.y = y; ym.m = m;
      }

      function renderCal(el, ym) {
        var first = new Date(ym.y, ym.m, 1);
        var lead = first.getDay(); // 0 = Sunday
        var daysInMonth = new Date(ym.y, ym.m + 1, 0).getDate();
        var daysInPrev = new Date(ym.y, ym.m, 0).getDate();
        var monthName = first.toLocaleString('en-US', { month: 'long', year: 'numeric' });
        var html = '<div class="mt-cal-head"><button type="button" data-nav="-1" aria-label="Previous month">&lt;</button>' +
          '<span>' + monthName + '</span><button type="button" data-nav="1" aria-label="Next month">&gt;</button></div>' +
          '<div class="mt-cal-grid">';
        DOW.forEach(function (d) { html += '<span class="mt-cal-dow">' + d + '</span>'; });
        var cells = [], i, d, t;
        for (i = lead - 1; i >= 0; i--) cells.push({ d: daysInPrev - i, other: true, off: -1 });
        for (d = 1; d <= daysInMonth; d++) cells.push({ d: d, other: false, off: 0 });
        var trail = (7 - (cells.length % 7)) % 7;
        for (t = 1; t <= trail; t++) cells.push({ d: t, other: true, off: 1 });
        cells.forEach(function (c) {
          var isoStr = toISO(new Date(ym.y, ym.m + c.off, c.d));
          var cls = 'mt-cal-day' + (c.other ? ' other-month' : '') +
            ((!c.other && (isoStr === startISO || isoStr === endISO)) ? ' selected' : '');
          html += '<button type="button" class="' + cls + '" data-date="' + isoStr + '">' + c.d + '</button>';
        });
        html += '</div>';
        el.innerHTML = html;
        el.querySelector('[data-nav="-1"]').addEventListener('click', function () { shift(ym, -1); renderBoth(); });
        el.querySelector('[data-nav="1"]').addEventListener('click', function () { shift(ym, 1); renderBoth(); });
        el.querySelectorAll('.mt-cal-day').forEach(function (btn) {
          btn.addEventListener('click', function () { pickDay(btn.getAttribute('data-date'), ym); });
        });
      }

      function renderBoth() {
        renderCal(qs('log-custom-cal-left'), left);
        renderCal(qs('log-custom-cal-right'), right);
      }

      function syncFields() {
        qs('log-custom-start').value = startISO;
        qs('log-custom-end').value = endISO;
      }

      function pickDay(isoStr, ym) {
        if (active === 'start') { startISO = isoStr; active = 'end'; }
        else { endISO = isoStr; active = 'start'; }
        // Clicking a greyed adjacent-month day moves that calendar there.
        var d = parseISO(isoStr);
        if (d && (d.getFullYear() !== ym.y || d.getMonth() !== ym.m)) {
          ym.y = d.getFullYear(); ym.m = d.getMonth();
        }
        syncFields();
        renderBoth();
      }

      function readField(id) {
        var v = (qs(id).value || '').trim();
        return parseISO(v) ? v : null;
      }

      return {
        open: function () {
          // Seed from the current range; default to today.
          var today = toISO(new Date());
          startISO = qs('log-custom-from').value || today;
          endISO = qs('log-custom-to').value || today;
          if (!parseISO(startISO)) startISO = today;
          if (!parseISO(endISO)) endISO = today;
          active = 'start';
          var s = parseISO(startISO), e = parseISO(endISO);
          left = { y: s.getFullYear(), m: s.getMonth() };
          right = { y: e.getFullYear(), m: e.getMonth() };
          if (left.y === right.y && left.m === right.m) shift(right, 1);
          syncFields();
          renderBoth();
          qs('log-custom-dialog-overlay').hidden = false;
          qs('log-custom-done').focus();
        },
        close: function () {
          qs('log-custom-dialog-overlay').hidden = true;
        },
        setActive: function (which) { active = (which === 'end') ? 'end' : 'start'; },
        fieldChanged: function (which) {
          var id = which === 'end' ? 'log-custom-end' : 'log-custom-start';
          var v = readField(id);
          if (v) {
            if (which === 'end') endISO = v; else startISO = v;
            renderBoth();
          } else {
            syncFields(); // revert invalid input
          }
        },
        apply: function () {
          var s = readField('log-custom-start'), e = readField('log-custom-end');
          if (!s || !e) { syncFields(); return; } // invalid: revert, stay open
          if (s > e) { var t = s; s = e; e = t; }
          startISO = s; endISO = e;
          qs('log-custom-from').value = s;
          qs('log-custom-to').value = e;
          this.close();
          loadLog();
        }
      };
    })();

    function currentFilters() {
      var period = currentPeriod().value;
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

    // KF-390: KanbanFlow puts the day before the month ("Friday, 16
    // October") and calls the current day "Today".
    function dayHeader(dateISO) {
      if (!dateISO) return 'Unknown date';
      if (dateISO === isoDate(new Date())) return 'Today';
      var d = new Date(dateISO + 'T12:00:00');
      return d.toLocaleDateString('en-US', { weekday: 'long' }) + ', ' + d.getDate() + ' ' +
        d.toLocaleDateString('en-US', { month: 'long' });
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
      return '<div class="log-neutral">Manually added time</div>';
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
          ' — ' + escapeHtml(fmtDuration(minutes, null)) + ' — ' + pomos + ' ' + pomoWord + '</span>' +
          '<button type="button" class="log-add-entry" data-open-manual-time' +
          ' data-date="' + escapeHtml(key) + '">Add time entry</button></div>';
        rows.forEach(function (e) {
          var dot = e.interrupted ? 'dot-orange' : 'dot-green';
          // KF-004: clicking a row opens the Edit entry dialog.
          html += '<div class="log-entry" data-entry-id="' + escapeHtml(e.id) + '">' +
            '<span class="timer-dot ' + dot + '"></span>' +
            '<span class="log-badge" title="' + escapeHtml(e.badge_title) + '">' +
            escapeHtml(e.badge_code) + '</span>' +
            '<div class="log-main"><div class="log-task">' + escapeHtml(e.task_name) + '</div>' +
            '<div class="log-when">' + escapeHtml(e.time_range) + '</div>' +
            entryStatus(e) +
            (e.note ? '<div class="log-note">' + escapeHtml(e.note) + '</div>' : '') +
            (e.labels && e.labels.length > 0 ? '<div class="log-labels">' + e.labels.map(function(l) { return '<span class="tm-label-chip">' + escapeHtml(l) + '</span>'; }).join('') + '</div>' : '') +
            '</div><div class="log-dur">' + escapeHtml(fmtDuration(e.minutes, e.duration_display)) + '</div></div>';
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

    // Shared row builder for the timer-log exports (KF-080: CSV + Excel).
    function logRows(entries) {
      var rows = [['Date', 'Task', 'Board', 'Type', 'Duration', 'Time range', 'Status', 'Note']];
      entries.forEach(function (e) {
        var status = e.kind === 'pomodoro'
          ? (e.interrupted
            ? "Stopped Pomodoro with reason '" + (e.interrupt_reason || '') + "'"
            : 'Successful Pomodoro')
          : (e.kind === 'stopwatch' ? 'Stopwatch session' : 'Manually added time');
        rows.push([
          e.day_key, e.task_name, e.board_name || '', e.kind,
          fmtDuration(e.minutes), e.time_range, status, e.note || '',
        ]);
      });
      return rows;
    }

    // KF-080: Excel export as an HTML-table .xls — Excel opens it natively,
    // no client library needed.
    function downloadExcel(filename, rows) {
      var html = '<html xmlns:o="urn:schemas-microsoft-com:office:office" ' +
        'xmlns:x="urn:schemas-microsoft-com:office:excel">' +
        '<head><meta charset="utf-8"></head><body><table>';
      rows.forEach(function (r) {
        html += '<tr>';
        r.forEach(function (c) {
          html += '<td>' + escapeHtml(String(c == null ? '' : c)) + '</td>';
        });
        html += '</tr>';
      });
      html += '</table></body></html>';
      var blob = new Blob(['﻿' + html], { type: 'application/vnd.ms-excel;charset=utf-8' });
      var a = document.createElement('a');
      a.href = URL.createObjectURL(blob);
      a.download = filename;
      document.body.appendChild(a);
      a.click();
      setTimeout(function () { URL.revokeObjectURL(a.href); a.remove(); }, 100);
    }

    // KF-289: KanbanFlow's timer log exports every entry as Excel or CSV.
    function exportLog(format) {
      fetchAll(currentFilters()).then(function (entries) {
        if (format === 'csv') downloadCsv('timer-log.csv', logRows(entries));
        else downloadExcel('timer-log.xls', logRows(entries));
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
      var board = qs('spent-board') && qs('spent-board').value;
      if (board) p.set('board_id', board);
      var entryType = qs('spent-entry-type') && qs('spent-entry-type').value;
      if (entryType) p.set('entry_type', entryType);
      list.innerHTML = '<p class="log-status">Loading&hellip;</p>';
      fetchJson('/api/timer/time-spent?' + p.toString()).then(function (rep) {
        var days = (rep.days || []).filter(function (d) { return d.minutes > 0; });
        var group = qs('spent-group').value;
        days.sort(function (a, b) {
          return group === 'date-asc' ? (a.date < b.date ? -1 : 1) : (a.date > b.date ? -1 : 1);
        });
        if (!days.length) {
          qs('spent-total').textContent = 'Total: ' + fmtDuration(rep.total_minutes || 0);
          list.innerHTML = '<p class="log-status">No entries exist for the given filter.</p>';
          return;
        }
        // KF-292: KanbanFlow-style table — date | bar+percent | tasks | breaks | duration%.
        var total = rep.total_minutes || 0;
        var html = '<table class="spent-table"><thead><tr>' +
          '<th>Date</th><th></th><th>Tasks</th><th>Breaks</th><th>Duration</th></tr></thead><tbody>';
        days.forEach(function (d) {
          var pct = total > 0 ? Math.round(d.minutes / total * 100) : 0;
          html += '<tr class="spent-row">' +
            '<td class="spent-date">' + escapeHtml(fmtDayLabel(d.date)) + '</td>' +
            '<td class="spent-bar-cell"><div class="spent-bar-track">' +
              '<div class="spent-bar-fill" style="width:' + pct + '%"></div></div>' +
              '<span class="spent-bar-pct">' + pct + '%</span></td>' +
            '<td class="spent-tasks">' + escapeHtml(fmtDuration(d.task_minutes || 0)) + '</td>' +
            '<td class="spent-breaks">' + escapeHtml(fmtDuration(d.break_minutes || 0)) + '</td>' +
            '<td class="spent-pct">' + pct + '%</td></tr>';
          if (TimeSpent.view === 'detailed') {
            html += '<tr class="spent-detail-row"><td colspan="5"><div class="spent-tasks-list">';
            (d.tasks || []).forEach(function (t) {
              html += '<div class="spent-task"><span class="spent-task-name">' +
                escapeHtml(t.task_name) + '</span>' +
                '<span class="spent-task-time">' + escapeHtml(fmtDuration(t.minutes)) + '</span></div>';
            });
            html += '</div></td></tr>';
          }
        });
        // Footer totals rows (KanbanFlow GM-123): "8h 4m / 100% / Total".
        var totalPct = 100;
        html += '</tbody><tfoot>' +
          '<tr class="spent-footer"><td>Total</td><td></td>' +
          '<td>' + escapeHtml(fmtDuration(rep.total_task_minutes || 0)) + '</td>' +
          '<td>' + escapeHtml(fmtDuration(rep.total_break_minutes || 0)) + '</td>' +
          '<td>' + totalPct + '%</td></tr>' +
          '<tr class="spent-footer-total"><td colspan="2"></td>' +
          '<td colspan="3">' + escapeHtml(fmtDuration(total)) + ' / ' + totalPct + '% / Total</td></tr>' +
          '</tfoot></table>';
        list.innerHTML = html;
        qs('spent-total').textContent = 'Total: ' + fmtDuration(total);
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

    // ---------- timer log modal (KF-286) ----------

    function openLogModal() {
      var overlay = qs('timer-log-overlay');
      if (!overlay) return;
      overlay.hidden = false;
      loadBoards();
      loadLog();
      var x = qs('log-close-x');
      if (x) x.focus();
    }

    function closeLogModal() {
      var overlay = qs('timer-log-overlay');
      if (overlay) overlay.hidden = true;
    }

    function loadBoards() {
      // Populate the "All boards" dropdown once; the board page only knows
      // the current board, so ask the API for the full list.
      if (boardsLoaded) return;
      var sel = qs('log-board-filter');
      if (!sel) return;
      boardsLoaded = true;
      fetchJson('/api/boards').then(function (boards) {
        (boards || []).forEach(function (b) {
          var opt = document.createElement('option');
          opt.value = b.id;
          opt.textContent = b.name;
          sel.appendChild(opt);
        });
      }).catch(function () { /* board filter is best-effort */ });
    }

    function initLogModal() {
      renderPeriodLabel();
      qs('log-period-prev').addEventListener('click', function () { stepPeriod(-1); });
      qs('log-period-next').addEventListener('click', function () { stepPeriod(1); });
      ['log-custom-from', 'log-custom-to', 'log-relative-n', 'log-relative-unit',
       'log-board-filter', 'log-type-filter'].forEach(function (id) {
        qs(id).addEventListener('change', loadLog);
      });
      qs('log-reload').addEventListener('click', loadLog);
      qs('log-print').addEventListener('click', function () { window.print(); });
      qs('log-settings').addEventListener('click', function () {
        // GM-117 caveat: the settings icon's destination was never observed
        // on the KanbanFlow modal; the timer settings modal is the adjacent,
        // most plausible target (same ⚙ glyph as the timer popup footer).
        if (window.TimerSettings) window.TimerSettings.open();
      });
      var exportBtn = qs('log-export');
      var exportMenu = qs('log-export-menu');
      exportBtn.addEventListener('click', function (e) {
        e.stopPropagation();
        exportMenu.hidden = !exportMenu.hidden;
      });
      document.addEventListener('click', function (e) {
        if (!e.target.closest('.export-wrap')) exportMenu.hidden = true;
      });
      exportMenu.addEventListener('click', function (e) {
        var item = e.target.closest('[data-export]');
        if (!item) return;
        exportMenu.hidden = true;
        exportLog(item.getAttribute('data-export'));
      });
      // Dismiss back to the board: header ×, icon-row ×, backdrop click, Escape.
      qs('log-close-x').addEventListener('click', closeLogModal);
      qs('log-close').addEventListener('click', closeLogModal);
      qs('timer-log-overlay').addEventListener('click', function (e) {
        if (e.target === this) closeLogModal();
      });
      document.addEventListener('keydown', function (e) {
        if (e.key !== 'Escape') return;
        // Dismiss the topmost layer first: the custom-range dialog, then the
        // export menu, then the modal.
        var cr = qs('log-custom-dialog-overlay');
        if (cr && !cr.hidden) { CustomRange.close(); return; }
        var menu = qs('log-export-menu');
        if (menu && !menu.hidden) { menu.hidden = true; return; }
        var overlay = qs('timer-log-overlay');
        if (overlay && !overlay.hidden) closeLogModal();
      });
      // KF-290: nested "Custom (absolute)" range dialog.
      qs('log-custom-done').addEventListener('click', function () { CustomRange.apply(); });
      qs('log-custom-back').addEventListener('click', function () { CustomRange.close(); });
      qs('log-custom-x').addEventListener('click', function () { CustomRange.close(); });
      qs('log-custom-dialog-overlay').addEventListener('click', function (e) {
        if (e.target === this) CustomRange.close();
      });
      qs('log-custom-start').addEventListener('focus', function () { CustomRange.setActive('start'); });
      qs('log-custom-end').addEventListener('focus', function () { CustomRange.setActive('end'); });
      qs('log-custom-start').addEventListener('change', function () { CustomRange.fieldChanged('start'); });
      qs('log-custom-end').addEventListener('change', function () { CustomRange.fieldChanged('end'); });
    }

    function initSpentPage() {
      qs('spent-reload').addEventListener('click', loadSpent);
      qs('spent-period').addEventListener('change', loadSpent);
      qs('spent-board').addEventListener('change', loadSpent);
      qs('spent-entry-type').addEventListener('change', loadSpent);
      qs('spent-color').addEventListener('change', loadSpent);
      qs('spent-group').addEventListener('change', loadSpent);
      qs('spent-print').addEventListener('click', openSpentPrintSheet);
      // KF-295: "Print" sheet (GM-169) — the report's Print button opens a
      // sheet titled "Print" with the report's task table and Filter / Print
      // buttons top-left. The sheet's Print button fires window.print() with
      // print CSS scoped to the sheet; Filter closes the sheet back to the
      // report (ChipFlow's filters are the always-visible row, KF-292).
      function openSpentPrintSheet() {
        var list = qs('spent-list');
        var body = qs('spent-print-body');
        if (list && body) body.innerHTML = list.innerHTML;
        qs('spent-print-overlay').hidden = false;
      }
      function closeSpentPrintSheet() {
        qs('spent-print-overlay').hidden = true;
      }
      qs('spent-print-do').addEventListener('click', function () { window.print(); });
      qs('spent-print-filter').addEventListener('click', closeSpentPrintSheet);
      qs('spent-print-overlay').addEventListener('click', function (e) {
        if (e.target === this) closeSpentPrintSheet();
      });
      document.addEventListener('keydown', function (e) {
        if (e.key !== 'Escape') return;
        var overlay = qs('spent-print-overlay');
        if (overlay && !overlay.hidden) closeSpentPrintSheet();
      });
      // KF-294: Time spent Export dropdown — verbatim KanbanFlow items
      // (GM-170): "Excel (Detailed)", "Excel (Summary)", "PDF (Summary)".
      var spentExportBtn = qs('spent-export');
      var spentExportMenu = qs('spent-export-menu');
      spentExportBtn.addEventListener('click', function (e) {
        e.stopPropagation();
        spentExportMenu.hidden = !spentExportMenu.hidden;
      });
      document.addEventListener('click', function (e) {
        if (!e.target.closest('.export-wrap')) spentExportMenu.hidden = true;
      });
      function fetchSpentReport() {
        var range = TimeSpent.periodRange();
        var p = new URLSearchParams({ from: range[0], to: range[1] });
        var color = qs('spent-color').value;
        if (color) p.set('color_id', color);
        var board = qs('spent-board') && qs('spent-board').value;
        if (board) p.set('board_id', board);
        var entryType = qs('spent-entry-type') && qs('spent-entry-type').value;
        if (entryType) p.set('entry_type', entryType);
        return fetchJson('/api/timer/time-spent?' + p.toString());
      }
      function spentSummaryRows(rep) {
        var days = (rep.days || []).filter(function (d) { return d.minutes > 0; });
        var rows = [['Date', 'Tasks', 'Breaks', 'Total']];
        days.forEach(function (d) {
          rows.push([
            fmtDayLabel(d.date),
            fmtDuration(d.task_minutes || 0),
            fmtDuration(d.break_minutes || 0),
            fmtDuration(d.minutes || 0),
          ]);
        });
        rows.push([
          'Total',
          fmtDuration(rep.total_task_minutes || 0),
          fmtDuration(rep.total_break_minutes || 0),
          fmtDuration(rep.total_minutes || 0),
        ]);
        return rows;
      }
      function spentDetailedRows(rep) {
        var days = (rep.days || []).filter(function (d) { return d.minutes > 0; });
        var rows = [['Date', 'Task', 'Time']];
        days.forEach(function (d) {
          (d.tasks || []).forEach(function (t) {
            rows.push([fmtDayLabel(d.date), t.task_name || '(no task)', fmtDuration(t.minutes || 0)]);
          });
        });
        return rows;
      }
      spentExportMenu.querySelector('[data-export="excel-detailed"]').addEventListener('click', function () {
        spentExportMenu.hidden = true;
        fetchSpentReport().then(function (rep) {
          downloadExcel('time-spent-detailed.xls', spentDetailedRows(rep));
        }).catch(function () { toast('Export failed.'); });
      });
      spentExportMenu.querySelector('[data-export="excel-summary"]').addEventListener('click', function () {
        spentExportMenu.hidden = true;
        fetchSpentReport().then(function (rep) {
          downloadExcel('time-spent-summary.xls', spentSummaryRows(rep));
        }).catch(function () { toast('Export failed.'); });
      });
      spentExportMenu.querySelector('[data-export="pdf-summary"]').addEventListener('click', function () {
        spentExportMenu.hidden = true;
        fetchSpentReport().then(function (rep) {
          var rows = spentSummaryRows(rep);
          var html = '<!DOCTYPE html><html><head><meta charset="utf-8">' +
            '<title>Time spent summary</title>' +
            '<style>body{font-family:sans-serif;margin:24px;color:#222}' +
            'h1{font-size:20px;margin:0 0 4px}p.sub{color:#666;font-size:13px;margin:0 0 16px}' +
            'table{border-collapse:collapse;width:100%}' +
            'th,td{border:1px solid #ccc;padding:6px 10px;text-align:left;font-size:13px}' +
            'th{background:#f5f5f5}tr.total td{font-weight:bold;background:#fafafa}' +
            '@media print{body{margin:12px}}</style></head><body>' +
            '<h1>Time spent — summary</h1>' +
            '<p class="sub">Total: ' + escapeHtml(fmtDuration(rep.total_minutes || 0)) + '</p>' +
            '<table><thead><tr>';
          rows[0].forEach(function (h) { html += '<th>' + escapeHtml(h) + '</th>'; });
          html += '</tr></thead><tbody>';
          for (var i = 1; i < rows.length; i++) {
            var cls = rows[i][0] === 'Total' ? ' class="total"' : '';
            html += '<tr' + cls + '>';
            rows[i].forEach(function (c) { html += '<td>' + escapeHtml(c) + '</td>'; });
            html += '</tr>';
          }
          html += '</tbody></table><scr' + 'ipt>window.onload=function(){window.print();}</scr' + 'ipt></body></html>';
          var win = window.open('', '_blank');
          if (!win) { toast('Please allow pop-ups to export PDF.'); return; }
          win.document.write(html);
          win.document.close();
        }).catch(function () { toast('Export failed.'); });
      });
      qs('spent-close').addEventListener('click', function () { window.location.href = '/'; });
      loadSpentBoards();
      loadSpentColors();
      loadSpent();
    }

    function loadSpentBoards() {
      // Populate the Board filter dropdown.
      var sel = qs('spent-board');
      if (!sel) return;
      fetchJson('/api/boards').then(function (boards) {
        (boards || []).forEach(function (b) {
          var opt = document.createElement('option');
          opt.value = b.id;
          opt.textContent = b.name;
          sel.appendChild(opt);
        });
      }).catch(function () { /* board filter is best-effort */ });
    }

    function init() {
      if (initialized) return;
      initialized = true;
      // KF-286: the timer log lives in a modal on the board page; the
      // Time spent report is its own page. Wire whichever is present.
      if (qs('timer-log-list')) initLogModal();
      if (qs('spent-list')) initSpentPage();
    }

    return { init: init, reload: loadLog, openModal: openLogModal, closeModal: closeLogModal };
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

    var MONTHS_SHORT = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun',
      'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

    // KF-293: KanbanFlow bar tooltip date — "Tuesday 1 Sep 2026"
    // (full weekday, day-of-month with no leading zero, abbreviated
    // month, 4-digit year; computed from the ISO date, not the
    // server "%b %d" axis label).
    function tooltipDate(dateISO) {
      var dt = new Date(dateISO + 'T12:00:00');
      return weekday(dateISO) + ' ' + dt.getDate() + ' ' +
        MONTHS_SHORT[dt.getMonth()] + ' ' + dt.getFullYear();
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
          // KF-293: count and minutes are separate fields — the count
          // drives the bar height and atop-bar label, the day's real
          // minutes drive the tooltip duration. Tooltip format per
          // KanbanFlow: "Tuesday 1 Sep 2026: 2 Pomodori (30m)" — the
          // word "Pomodori" is used verbatim for any count.
          return {
            label: d.label,
            count: d.pomodori,
            minutes: d.minutes,
            tooltip: tooltipDate(d.date) + ': ' + d.pomodori + ' Pomodori (' +
              fmtDuration(d.minutes) + ')',
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

      // KF-347: Break activities tab — per-activity totals for the range
      // plus today's progress against each activity's daily goal.
      var bas = rep.break_activities || [];
      var bh;
      if (!bas.length) {
        bh = '<p class="log-status">No break activities defined</p>';
      } else {
        var bmax = 1;
        bas.forEach(function (a) { if (a.minutes > bmax) bmax = a.minutes; });
        bh = '';
        bas.forEach(function (a) {
          var goalTxt = '';
          if (a.daily_goal > 0) {
            goalTxt = ' <span class="reason-meta">today ' + fmtDuration(a.today_minutes) +
              ' / ' + fmtDuration(a.daily_goal) + ' goal</span>';
          }
          bh += '<div class="reason-row"><span class="reason-name">' + escapeHtml(a.name) +
            '</span><div class="reason-bar"><div class="reason-fill" style="width:' +
            Math.round(100 * a.minutes / bmax) + '%"></div></div>' +
            '<span class="reason-count">' + fmtDuration(a.minutes) + goalTxt + '</span></div>';
        });
      }
      qs('stats-breaks').innerHTML = bh;

      // Highscores tab. KF-391: no best day means nothing was logged.
      var hs = '<p class="log-status">No data to display</p>';
      if (rep.best_day) {
        hs = statCard(rep.best_day.pomodori,
          'Best day — ' + weekday(rep.best_day.date) + ', ' + rep.best_day.label) +
          statCard(rep.longest_streak, 'Longest streak (days)');
      }
      qs('stats-highscores').innerHTML = hs;
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

  // KF-346: Export report page — download the board's tasks as a file.
  // Formats match KanbanFlow's documented export targets: Excel, CSV,
  // XML, JSON. The board dataset comes from /api/boards/:id/export-data
  // (the shared data shape for the board reports); the file is rendered
  // client-side, same as the timer-log exports.
  var ExportPage = (function () {
    var initialized = false;

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('export-status');
      if (el) el.textContent = msg;
    }

    // Column set for the tabular formats (Excel/CSV).
    function exportColumns() {
      return ['Task', 'Description', 'Column', 'Swimlane', 'Color', 'Labels',
        'Members', 'Due date', 'Due done', 'Estimate (h)', 'Size (pomodori)',
        'Time spent (min)', 'Pomodoros', 'Interruptions', 'Created at',
        'Completed at', 'Added to column at', 'Subtasks', 'Comments'];
    }

    function lookupMap(list) {
      var m = {};
      (list || []).forEach(function (e) { m[e.id] = e; });
      return m;
    }

    // Resolve one task row to display strings using the lookup tables.
    function taskRecord(t, lookups) {
      var col = lookups.columns[t.column_id];
      var lane = t.swimlane_id ? lookups.swimlanes[t.swimlane_id] : null;
      var color = t.color_id ? lookups.colors[t.color_id] : null;
      var members = (t.member_ids || []).map(function (id) {
        var u = lookups.users[id];
        return u ? u.username : id;
      });
      var doneSub = (t.subtasks || []).filter(function (s) { return s.done; }).length;
      return {
        name: t.name || '',
        description: t.description || '',
        column: col ? col.name : '',
        swimlane: lane ? lane.name : '',
        color: color ? (color.label || color.value) : '',
        labels: (t.labels || []).join(', '),
        members: members.join(', '),
        due_at: t.due_at || '',
        due_done: t.due_done ? 'yes' : '',
        estimate_hours: t.estimate_hours == null ? '' : String(t.estimate_hours),
        size: t.size == null ? '' : String(t.size),
        total_minutes: String(t.total_minutes || 0),
        pomodori_completed: String(t.pomodori_completed || 0),
        interruptions: String(t.interruptions || 0),
        created_at: t.created_at || '',
        completed_at: t.completed_at || '',
        column_added_at: t.column_added_at || '',
        subtasks: doneSub + '/' + (t.subtasks || []).length,
        comments: String((t.comments || []).length)
      };
    }

    function recordToRow(r) {
      return [r.name, r.description, r.column, r.swimlane, r.color, r.labels,
        r.members, r.due_at, r.due_done, r.estimate_hours, r.size,
        r.total_minutes, r.pomodori_completed, r.interruptions, r.created_at,
        r.completed_at, r.column_added_at, r.subtasks, r.comments];
    }

    function download(filename, content, mime) {
      var blob = new Blob([content], { type: mime });
      var a = document.createElement('a');
      a.href = URL.createObjectURL(blob);
      a.download = filename;
      document.body.appendChild(a);
      a.click();
      setTimeout(function () { URL.revokeObjectURL(a.href); a.remove(); }, 100);
    }

    function sanitizeFilename(name) {
      var s = String(name || 'board').replace(/[^A-Za-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
      return s || 'board';
    }

    function downloadExcel(filename, rows) {
      // Same HTML-table .xls trick as the timer-log exports: Excel opens
      // it natively, no client library needed.
      var html = '<html xmlns:o="urn:schemas-microsoft-com:office:office" ' +
        'xmlns:x="urn:schemas-microsoft-com:office:excel">' +
        '<head><meta charset="utf-8"></head><body><table>';
      rows.forEach(function (r) {
        html += '<tr>';
        r.forEach(function (c) {
          html += '<td>' + escapeHtml(String(c == null ? '' : c)) + '</td>';
        });
        html += '</tr>';
      });
      html += '</table></body></html>';
      download(filename, '﻿' + html, 'application/vnd.ms-excel;charset=utf-8');
    }

    function csvCell(v) {
      var s = String(v == null ? '' : v);
      return /[",\n\r]/.test(s) ? '"' + s.replace(/"/g, '""') + '"' : s;
    }

    function downloadCsv(filename, rows) {
      var out = rows.map(function (r) { return r.map(csvCell).join(','); }).join('\r\n');
      download(filename, '﻿' + out, 'text/csv;charset=utf-8');
    }

    function xmlEscape(s) {
      return String(s == null ? '' : s)
        .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;').replace(/'/g, '&apos;');
    }

    function downloadXml(filename, records, boardName) {
      var xml = '<?xml version="1.0" encoding="UTF-8"?>\n<tasks board="' +
        xmlEscape(boardName) + '">\n';
      records.forEach(function (r) {
        xml += '  <task>\n';
        Object.keys(r).forEach(function (k) {
          var tag = k.replace(/[^A-Za-z0-9_]/g, '_');
          xml += '    <' + tag + '>' + xmlEscape(r[k]) + '</' + tag + '>\n';
        });
        xml += '  </task>\n';
      });
      xml += '</tasks>\n';
      download(filename, xml, 'application/xml;charset=utf-8');
    }

    function downloadJson(filename, records, boardName) {
      download(filename,
        JSON.stringify({ board: boardName, tasks: records }, null, 2) + '\n',
        'application/json;charset=utf-8');
    }

    function doExport(boardId, format) {
      setStatus('Preparing export…');
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data',
        { headers: { 'Accept': 'application/json' } })
        .then(function (resp) {
          if (!resp.ok) throw new Error('HTTP ' + resp.status);
          return resp.json();
        })
        .then(function (data) {
          var lookups = {
            columns: lookupMap(data.columns),
            swimlanes: lookupMap(data.swimlanes),
            colors: lookupMap(data.colors),
            users: lookupMap(data.users)
          };
          var records = (data.tasks || []).map(function (t) { return taskRecord(t, lookups); });
          var base = sanitizeFilename(data.board && data.board.name) + '-export';
          var rows;
          if (format === 'excel') {
            rows = [exportColumns()];
            records.forEach(function (r) { rows.push(recordToRow(r)); });
            downloadExcel(base + '.xls', rows);
          } else if (format === 'csv') {
            rows = [exportColumns()];
            records.forEach(function (r) { rows.push(recordToRow(r)); });
            downloadCsv(base + '.csv', rows);
          } else if (format === 'xml') {
            downloadXml(base + '.xml', records, data.board && data.board.name);
          } else {
            downloadJson(base + '.json', records, data.board && data.board.name);
          }
          setStatus('Exported ' + records.length +
            ' task' + (records.length === 1 ? '' : 's') + '.');
        })
        .catch(function (err) {
          setStatus('Export failed: ' + (err && err.message ? err.message : err));
        });
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      var close = qs('export-close');
      if (close) close.addEventListener('click', function () {
        window.location.href = '/b/' + encodeURIComponent(boardId);
      });
      var buttons = document.querySelectorAll('[data-export-format]');
      buttons.forEach(function (btn) {
        btn.addEventListener('click', function () {
          doExport(boardId, btn.getAttribute('data-export-format'));
        });
      });
    }

    return { init: init };
  })();

  // KF-343: Task count report page — number of tasks in each column,
  // broken down by color (KanbanFlow's "Time estimate & Task count" layout).
  // Renders client-side from the shared export-data dataset.
  var TaskCountPage = (function () {
    var initialized = false;

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('taskcount-status');
      if (el) el.textContent = msg;
    }

    // Returns { columns, rows } where rows are per color (plus a "No color"
    // row when needed) with per-column counts. Colors with no tasks are
    // omitted; row order follows first appearance on the board.
    function buildTable(data) {
      var columns = data.columns || [];
      var colors = data.colors || [];
      var tasks = data.tasks || [];
      var colorById = {};
      colors.forEach(function (c) { colorById[c.id] = c; });
      var colIndex = {};
      columns.forEach(function (c, i) { colIndex[c.id] = i; });

      var counts = {};
      var order = [];
      tasks.forEach(function (t) {
        var key = t.color_id || '';
        if (order.indexOf(key) === -1) order.push(key);
        if (!counts[key]) counts[key] = columns.map(function () { return 0; });
        var i = colIndex[t.column_id];
        if (i !== undefined) counts[key][i]++;
      });

      var rows = order.map(function (key) {
        var c = colorById[key];
        return {
          label: c ? (c.label || c.value) : 'No color',
          value: c ? c.value : null,
          counts: counts[key]
        };
      });
      return { columns: columns, rows: rows };
    }

    function render(table) {
      var host = qs('taskcount-table');
      if (!host) return;
      if (!table.rows.length) {
        host.innerHTML = '<p class="log-status">No tasks on this board.</p>';
        return;
      }
      var html = '<table class="report-table"><thead><tr><th scope="col"><span class="sr-only">Color</span></th>';
      table.columns.forEach(function (c) {
        html += '<th scope="col">' + escapeHtml(c.name) + '</th>';
      });
      html += '</tr></thead><tbody>';
      table.rows.forEach(function (r) {
        html += '<tr><th scope="row"><span class="report-color-dot"' +
          (r.value
            ? ' style="background:' + escapeHtml(r.value) + '"'
            : ' style="background:transparent;border:1px dashed #9ca3af"') +
          '></span> ' + escapeHtml(r.label) + '</th>';
        r.counts.forEach(function (n) {
          html += '<td>' + n + '</td>';
        });
        html += '</tr>';
      });
      html += '</tbody></table>';
      host.innerHTML = html;
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      var close = qs('taskcount-close');
      if (close) close.addEventListener('click', function () {
        window.location.href = '/b/' + encodeURIComponent(boardId);
      });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' }
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        var table = buildTable(data);
        render(table);
        var total = table.rows.reduce(function (s, r) {
          return s + r.counts.reduce(function (a, b) { return a + b; }, 0);
        }, 0);
        setStatus(total === 1 ? '1 task.' : total + ' tasks.');
      }).catch(function (err) {
        setStatus('Could not load task data: ' + err.message);
      });
    }

    return { init: init };
  })();

  // KF-344: Throughput report page — tasks completed per week as a line
  // chart (KanbanFlow's Throughput chart). Renders client-side from the
  // shared export-data dataset; pure inline SVG, no chart library.
  var ThroughputPage = (function () {
    var initialized = false;
    var WEEKS = 12;

    var MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun',
      'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('throughput-status');
      if (el) el.textContent = msg;
    }

    // Monday (local) of the week containing d.
    function weekStart(d) {
      var x = new Date(d.getFullYear(), d.getMonth(), d.getDate());
      x.setDate(x.getDate() - ((x.getDay() + 6) % 7));
      return x;
    }

    function fmtDay(d) {
      return d.getDate() + '. ' + MONTHS[d.getMonth()];
    }

    // Bucket completed_at timestamps into the last WEEKS week buckets
    // (oldest first). Unparseable dates are ignored.
    function bucketByWeek(tasks) {
      var now = weekStart(new Date());
      var starts = [];
      for (var i = WEEKS - 1; i >= 0; i--) {
        var s = new Date(now);
        s.setDate(s.getDate() - 7 * i);
        starts.push(s.getTime());
      }
      var counts = starts.map(function () { return 0; });
      tasks.forEach(function (t) {
        if (!t.completed_at) return;
        var d = new Date(t.completed_at);
        if (isNaN(d.getTime())) return;
        var idx = starts.indexOf(weekStart(d).getTime());
        if (idx !== -1) counts[idx]++;
      });
      return {
        starts: starts.map(function (ms) { return new Date(ms); }),
        counts: counts
      };
    }

    function renderChart(buckets) {
      var host = qs('throughput-chart');
      if (!host) return;
      var W = 720, H = 300, padL = 46, padR = 32, padT = 16, padB = 34;
      var innerW = W - padL - padR, innerH = H - padT - padB;
      var max = 1;
      buckets.counts.forEach(function (n) { if (n > max) max = n; });
      max = Math.ceil(max / 5) * 5;

      var n = buckets.counts.length;
      function x(i) { return padL + (n <= 1 ? innerW / 2 : (i / (n - 1)) * innerW); }
      function y(v) { return padT + innerH - (v / max) * innerH; }

      var svg = '<svg viewBox="0 0 ' + W + ' ' + H + '" class="line-chart">';
      var ticks = 5, t, v, yy;
      for (t = 0; t <= ticks; t++) {
        v = Math.round(max * t / ticks);
        yy = y(v).toFixed(1);
        svg += '<line x1="' + padL + '" y1="' + yy + '" x2="' + (W - padR) +
          '" y2="' + yy + '" class="chart-grid"/>' +
          '<text x="' + (padL - 8) + '" y="' + (Number(yy) + 4) +
          '" text-anchor="end" class="chart-tick">' + v + '</text>';
      }
      var midY = padT + innerH / 2;
      svg += '<text x="14" y="' + midY + '" text-anchor="middle" class="chart-axis-label"' +
        ' transform="rotate(-90 14 ' + midY + ')">Task count</text>';
      var i, j, k;
      for (i = 0; i < n; i++) {
        svg += '<text x="' + x(i).toFixed(1) + '" y="' + (H - 10) +
          '" text-anchor="middle" class="chart-tick">' +
          escapeHtml(fmtDay(buckets.starts[i])) + '</text>';
      }
      var pts = [];
      for (j = 0; j < n; j++) {
        pts.push(x(j).toFixed(1) + ',' + y(buckets.counts[j]).toFixed(1));
      }
      svg += '<polyline points="' + pts.join(' ') + '" class="chart-line" fill="none"/>';
      for (k = 0; k < n; k++) {
        svg += '<circle cx="' + x(k).toFixed(1) + '" cy="' + y(buckets.counts[k]).toFixed(1) +
          '" r="4" class="chart-dot"><title>' + buckets.counts[k] +
          ' tasks, week of ' + escapeHtml(fmtDay(buckets.starts[k])) + '</title></circle>';
      }
      svg += '</svg>';
      host.innerHTML = svg;
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      var close = qs('throughput-close');
      if (close) close.addEventListener('click', function () {
        window.location.href = '/b/' + encodeURIComponent(boardId);
      });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' }
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        var buckets = bucketByWeek(data.tasks || []);
        renderChart(buckets);
        var total = buckets.counts.reduce(function (a, b) { return a + b; }, 0);
        setStatus(total === 1
          ? '1 task completed in the last 12 weeks.'
          : total + ' tasks completed in the last 12 weeks.');
      }).catch(function (err) {
        setStatus('Could not load task data: ' + err.message);
      });
    }

    return { init: init };
  })();

  var BurndownPage = (function () {
    var initialized = false;
    var DAYS = 30;

    var MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun',
      'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('burndown-status');
      if (el) el.textContent = msg;
    }

    function fmtDay(d) {
      return d.getDate() + '. ' + MONTHS[d.getMonth()];
    }

    // Day-ends (local) for the last DAYS days, oldest first.
    function dayEnds() {
      var ends = [];
      var today = new Date();
      today.setHours(23, 59, 59, 999);
      for (var i = DAYS - 1; i >= 0; i--) {
        var d = new Date(today);
        d.setDate(d.getDate() - i);
        ends.push(d);
      }
      return ends;
    }

    // Remaining tasks at each day-end: created on/before the day-end and
    // not completed (completed_at null or after the day-end). Unparseable
    // dates are ignored.
    function remainingByDay(tasks) {
      var ends = dayEnds();
      var parsed = tasks.map(function (t) {
        var c = new Date(t.created_at);
        var done = t.completed_at ? new Date(t.completed_at) : null;
        return {
          c: isNaN(c.getTime()) ? null : c.getTime(),
          d: done && !isNaN(done.getTime()) ? done.getTime() : null
        };
      }).filter(function (t) { return t.c !== null; });
      return {
        ends: ends,
        counts: ends.map(function (e) {
          var ms = e.getTime();
          var n = 0;
          parsed.forEach(function (t) {
            if (t.c <= ms && (t.d === null || t.d > ms)) n++;
          });
          return n;
        })
      };
    }

    function renderChart(series) {
      var host = qs('burndown-chart');
      if (!host) return;
      var W = 720, H = 300, padL = 46, padR = 32, padT = 16, padB = 34;
      var innerW = W - padL - padR, innerH = H - padT - padB;
      var max = 1;
      series.counts.forEach(function (n) { if (n > max) max = n; });
      max = Math.ceil(max / 5) * 5;

      var n = series.counts.length;
      function x(i) { return padL + (n <= 1 ? innerW / 2 : (i / (n - 1)) * innerW); }
      function y(v) { return padT + innerH - (v / max) * innerH; }

      var svg = '<svg viewBox="0 0 ' + W + ' ' + H + '" class="line-chart">';
      var ticks = 5, t, v, yy;
      for (t = 0; t <= ticks; t++) {
        v = Math.round(max * t / ticks);
        yy = y(v).toFixed(1);
        svg += '<line x1="' + padL + '" y1="' + yy + '" x2="' + (W - padR) +
          '" y2="' + yy + '" class="chart-grid"/>' +
          '<text x="' + (padL - 8) + '" y="' + (Number(yy) + 4) +
          '" text-anchor="end" class="chart-tick">' + v + '</text>';
      }
      var midY = padT + innerH / 2;
      svg += '<text x="14" y="' + midY + '" text-anchor="middle" class="chart-axis-label"' +
        ' transform="rotate(-90 14 ' + midY + ')">Tasks remaining</text>';
      var i, j, k;
      for (i = 0; i < n; i += 5) {
        svg += '<text x="' + x(i).toFixed(1) + '" y="' + (H - 10) +
          '" text-anchor="middle" class="chart-tick">' +
          escapeHtml(fmtDay(series.ends[i])) + '</text>';
      }
      var pts = [];
      for (j = 0; j < n; j++) {
        pts.push(x(j).toFixed(1) + ',' + y(series.counts[j]).toFixed(1));
      }
      svg += '<polyline points="' + pts.join(' ') + '" class="chart-line" fill="none"/>';
      for (k = 0; k < n; k++) {
        svg += '<circle cx="' + x(k).toFixed(1) + '" cy="' + y(series.counts[k]).toFixed(1) +
          '" r="4" class="chart-dot"><title>' + series.counts[k] +
          ' tasks remaining, ' + escapeHtml(fmtDay(series.ends[k])) + '</title></circle>';
      }
      svg += '</svg>';
      host.innerHTML = svg;
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      var close = qs('burndown-close');
      if (close) close.addEventListener('click', function () {
        window.location.href = '/b/' + encodeURIComponent(boardId);
      });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' }
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        var series = remainingByDay(data.tasks || []);
        renderChart(series);
        var current = series.counts.length ? series.counts[series.counts.length - 1] : 0;
        setStatus((data.tasks || []).length === 0
          ? 'No tasks on this board.'
          : (current === 1 ? '1 task remaining.' : current + ' tasks remaining.'));
      }).catch(function (err) {
        setStatus('Could not load task data: ' + err.message);
      });
    }

    return { init: init };
  })();

  // KF-340: Cycle & lead time report page. Average cycle/lead time per week
  // (Monday-start, last 12 weeks) with a 3-week moving-average trend line,
  // rendered from the shared export-data dataset.
  // Definitions (KanbanFlow): lead time = creation -> completion; cycle time
  // = first column move -> completion (falls back to creation when the task
  // was never moved).
  var CycleLeadPage = (function () {
    var initialized = false;
    var WEEKS = 12;
    var metric = 'cycle';

    var MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun',
      'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('cycle-lead-status');
      if (el) el.textContent = msg;
    }

    // Monday (local) of the week containing d.
    function weekStart(d) {
      var x = new Date(d.getFullYear(), d.getMonth(), d.getDate());
      x.setDate(x.getDate() - ((x.getDay() + 6) % 7));
      return x;
    }

    function fmtDay(d) {
      return d.getDate() + '. ' + MONTHS[d.getMonth()];
    }

    function daysBetween(a, b) {
      var d = (b.getTime() - a.getTime()) / 86400000;
      return d < 0 ? 0 : d;
    }

    // Per completed task: completion week + cycle/lead days.
    function computeTimes(tasks) {
      var out = [];
      tasks.forEach(function (t) {
        if (!t.completed_at) return;
        var created = new Date(t.created_at);
        var done = new Date(t.completed_at);
        if (isNaN(created.getTime()) || isNaN(done.getTime())) return;
        var started = created;
        (t.history || []).forEach(function (e) {
          if (e && e.kind === 'moved') {
            var d = new Date(e.created_at);
            if (!isNaN(d.getTime()) && d.getTime() < started.getTime()) started = d;
          }
        });
        out.push({
          doneWeek: weekStart(done).getTime(),
          cycle: daysBetween(started, done),
          lead: daysBetween(created, done)
        });
      });
      return out;
    }

    // 12 Monday-start week buckets (oldest first); each holds sum/count
    // per metric so the weekly average can be computed.
    function bucketize(times) {
      var now = weekStart(new Date());
      var starts = [];
      for (var i = WEEKS - 1; i >= 0; i--) {
        var s = new Date(now);
        s.setDate(s.getDate() - 7 * i);
        starts.push(s.getTime());
      }
      var buckets = starts.map(function () {
        return { cycleSum: 0, cycleN: 0, leadSum: 0, leadN: 0 };
      });
      times.forEach(function (t) {
        var idx = starts.indexOf(t.doneWeek);
        if (idx === -1) return;
        buckets[idx].cycleSum += t.cycle; buckets[idx].cycleN++;
        buckets[idx].leadSum += t.lead; buckets[idx].leadN++;
      });
      return {
        starts: starts.map(function (ms) { return new Date(ms); }),
        avg: buckets.map(function (b) {
          return {
            cycle: b.cycleN ? b.cycleSum / b.cycleN : 0,
            lead: b.leadN ? b.leadSum / b.leadN : 0,
            n: b.cycleN
          };
        })
      };
    }

    // Centered moving average over the series (edges use available points).
    function movingAverage(vals) {
      var out = [];
      for (var i = 0; i < vals.length; i++) {
        var sum = 0, n = 0;
        for (var j = i - 1; j <= i + 1; j++) {
          if (j >= 0 && j < vals.length) { sum += vals[j]; n++; }
        }
        out.push(n ? sum / n : 0);
      }
      return out;
    }

    function fmtDays(v) {
      return (Math.round(v * 10) / 10) + ' days';
    }

    function renderChart(data) {
      var host = qs('cycle-lead-chart');
      if (!host) return;
      var W = 720, H = 300, padL = 52, padR = 32, padT = 16, padB = 34;
      var innerW = W - padL - padR, innerH = H - padT - padB;
      var vals = data.avg.map(function (a) { return a[metric]; });
      var trend = movingAverage(vals);
      var max = 1;
      vals.concat(trend).forEach(function (v) { if (v > max) max = v; });
      max = Math.ceil(max / 10) * 10 || 10;

      var n = vals.length;
      function x(i) { return padL + (n <= 1 ? innerW / 2 : (i / (n - 1)) * innerW); }
      function y(v) { return padT + innerH - (v / max) * innerH; }

      var svg = '<svg viewBox="0 0 ' + W + ' ' + H + '" class="line-chart">';
      var t, v, yy;
      for (t = 0; t <= 5; t++) {
        v = Math.round(max * t / 5 * 10) / 10;
        yy = y(v).toFixed(1);
        svg += '<line x1="' + padL + '" y1="' + yy + '" x2="' + (W - padR) +
          '" y2="' + yy + '" class="chart-grid"/>' +
          '<text x="' + (padL - 8) + '" y="' + (Number(yy) + 4) +
          '" text-anchor="end" class="chart-tick">' + v + '</text>';
      }
      var midY = padT + innerH / 2;
      svg += '<text x="16" y="' + midY + '" text-anchor="middle" class="chart-axis-label"' +
        ' transform="rotate(-90 16 ' + midY + ')">Average time (days)</text>';
      var i, j, k;
      for (i = 0; i < n; i++) {
        svg += '<text x="' + x(i).toFixed(1) + '" y="' + (H - 10) +
          '" text-anchor="middle" class="chart-tick">' +
          escapeHtml(fmtDay(data.starts[i])) + '</text>';
      }
      // Trend (moving average): dashed gray line under the metric line.
      var tp = [];
      for (j = 0; j < n; j++) {
        tp.push(x(j).toFixed(1) + ',' + y(trend[j]).toFixed(1));
      }
      svg += '<polyline points="' + tp.join(' ') + '" class="chart-trend" fill="none"/>';
      var pts = [];
      for (j = 0; j < n; j++) {
        pts.push(x(j).toFixed(1) + ',' + y(vals[j]).toFixed(1));
      }
      svg += '<polyline points="' + pts.join(' ') + '" class="chart-line" fill="none"/>';
      for (k = 0; k < n; k++) {
        svg += '<circle cx="' + x(k).toFixed(1) + '" cy="' + y(vals[k]).toFixed(1) +
          '" r="4" class="chart-dot"><title>' + escapeHtml(fmtDays(vals[k])) +
          ' avg, week of ' + escapeHtml(fmtDay(data.starts[k])) +
          ' (' + data.avg[k].n + (data.avg[k].n === 1 ? ' task' : ' tasks') + ')</title></circle>';
      }
      svg += '</svg>';
      host.innerHTML = svg;
    }

    var lastData = null;

    function setMetric(m) {
      metric = m;
      var cb = qs('cycle-lead-cycle'), lb = qs('cycle-lead-lead');
      if (cb) { cb.classList.toggle('report-toggle-active', m === 'cycle'); cb.setAttribute('aria-pressed', m === 'cycle' ? 'true' : 'false'); }
      if (lb) { lb.classList.toggle('report-toggle-active', m === 'lead'); lb.setAttribute('aria-pressed', m === 'lead' ? 'true' : 'false'); }
      if (lastData) renderChart(lastData);
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      var close = qs('cycle-lead-close');
      if (close) close.addEventListener('click', function () {
        window.location.href = '/b/' + encodeURIComponent(boardId);
      });
      var cb = qs('cycle-lead-cycle'), lb = qs('cycle-lead-lead');
      if (cb) cb.addEventListener('click', function () { setMetric('cycle'); });
      if (lb) lb.addEventListener('click', function () { setMetric('lead'); });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' }
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        var times = computeTimes(data.tasks || []);
        var buckets = bucketize(times);
        lastData = buckets;
        renderChart(buckets);
        var total = times.length;
        setStatus(total === 0
          ? 'No completed tasks in the last 12 weeks.'
          : (total === 1 ? '1 task completed in the last 12 weeks.'
                         : total + ' tasks completed in the last 12 weeks.'));
      }).catch(function (err) {
        setStatus('Could not load task data: ' + err.message);
      });
    }

    return { init: init };
  })();
  // KF-339: Cumulative flow report page — tasks in each column over time
  // as a stacked area chart. Reconstructed from each task's history trail
  // ("created"/"moved" events), falling back to the task's current column
  // when it has no recorded moves.
  var CumulativeFlowPage = (function () {
    var initialized = false;
    var DAYS = 30;

    var MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun',
      'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

    // One color per column band.
    var PALETTE = ['#4e79a7', '#f28e2b', '#e15759', '#76b7b2', '#59a14f',
      '#edc948', '#b07aa1', '#ff9da7', '#9c755f', '#bab0ac'];

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('cumulative-flow-status');
      if (el) el.textContent = msg;
    }

    function fmtDay(d) {
      return d.getDate() + '. ' + MONTHS[d.getMonth()];
    }

    function parseMoveDetail(detail) {
      // "Moved from {from} to {to}" — split on the last " to " so column
      // names containing " to " parse deterministically.
      var body = detail.slice('Moved from '.length);
      var i = body.lastIndexOf(' to ');
      if (i === -1) return null;
      return { from: body.slice(0, i), to: body.slice(i + 4) };
    }

    // Per-task column timeline: [{at: ms, col: name}] sorted ascending.
    // "created" events carry the initial column; "moved" events carry the
    // destination. Tasks with no usable events fall back to their current
    // column for the whole window.
    function timelineFor(task, colNameById) {
      var tl = [];
      (task.history || []).forEach(function (e) {
        var d = new Date(e.created_at);
        if (isNaN(d.getTime())) return;
        if (e.kind === 'created' && e.detail.indexOf('Task created in ') === 0) {
          tl.push({ at: d.getTime(), col: e.detail.slice('Task created in '.length) });
        } else if (e.kind === 'moved' && e.detail.indexOf('Moved from ') === 0) {
          var m = parseMoveDetail(e.detail);
          if (m) tl.push({ at: d.getTime(), col: m.to });
        }
      });
      tl.sort(function (a, b) { return a.at - b.at; });
      if (tl.length === 0) {
        var c = new Date(task.created_at);
        var fallback = colNameById[task.column_id] || null;
        if (!isNaN(c.getTime()) && fallback) tl.push({ at: c.getTime(), col: fallback });
      }
      // KF-350: the timeline's tail is the task's latest position. History
      // detail strings freeze the column name at event time, so a column
      // renamed after the task's last move would leave a phantom band under
      // the old name on the current day while the renamed column shows 0.
      // Prefer the column's CURRENT name (colNameById[task.column_id]) for
      // the final entry.
      var current = colNameById[task.column_id];
      if (current && tl.length > 0) tl[tl.length - 1].col = current;
      return tl;
    }

    // Column order: the board's runtime columns first, then any extra
    // names seen in history (renamed/deleted columns) in first-appearance
    // order, so the chart always sums to the true total.
    function columnOrder(data) {
      var order = (data.columns || []).map(function (c) { return c.name; });
      var seen = {};
      order.forEach(function (n) { seen[n] = true; });
      (data.tasks || []).forEach(function (t) {
        (t.history || []).forEach(function (e) {
          var names = [];
          if (e.kind === 'created' && e.detail.indexOf('Task created in ') === 0) {
            names.push(e.detail.slice('Task created in '.length));
          } else if (e.kind === 'moved' && e.detail.indexOf('Moved from ') === 0) {
            var m = parseMoveDetail(e.detail);
            if (m) { names.push(m.from); names.push(m.to); }
          }
          names.forEach(function (nm) {
            if (!seen[nm]) { seen[nm] = true; order.push(nm); }
          });
        });
      });
      return order;
    }

    function seriesByDay(data) {
      var ends = [];
      var today = new Date();
      today.setHours(23, 59, 59, 999);
      for (var i = DAYS - 1; i >= 0; i--) {
        var d = new Date(today);
        d.setDate(d.getDate() - i);
        ends.push(d);
      }
      var colNameById = {};
      (data.columns || []).forEach(function (c) { colNameById[c.id] = c.name; });
      var order = columnOrder(data);
      var idx = {};
      order.forEach(function (n, k) { idx[n] = k; });
      var counts = order.map(function () {
        return ends.map(function () { return 0; });
      });
      (data.tasks || []).forEach(function (t) {
        var tl = timelineFor(t, colNameById);
        if (tl.length === 0) return;
        var p = 0;
        ends.forEach(function (e, di) {
          var ms = e.getTime();
          while (p + 1 < tl.length && tl[p + 1].at <= ms) p++;
          if (tl[p].at <= ms && idx[tl[p].col] !== undefined) counts[idx[tl[p].col]][di]++;
        });
      });
      return { ends: ends, order: order, counts: counts };
    }

    function renderChart(s) {
      var host = qs('cumulative-flow-chart');
      if (!host) return;
      var W = 720, H = 320, padL = 46, padR = 32, padT = 16, padB = 34;
      var innerW = W - padL - padR, innerH = H - padT - padB;
      var n = s.ends.length;
      var totals = s.ends.map(function (_, di) {
        var t = 0;
        s.counts.forEach(function (c) { t += c[di]; });
        return t;
      });
      var max = 1;
      totals.forEach(function (t) { if (t > max) max = t; });
      max = Math.ceil(max / 5) * 5;

      function x(i) { return padL + (n <= 1 ? innerW / 2 : (i / (n - 1)) * innerW); }
      function y(v) { return padT + innerH - (v / max) * innerH; }

      var svg = '<svg viewBox="0 0 ' + W + ' ' + H + '" class="line-chart">';
      var ticks = 5, t, v, yy;
      for (t = 0; t <= ticks; t++) {
        v = Math.round(max * t / ticks);
        yy = y(v).toFixed(1);
        svg += '<line x1="' + padL + '" y1="' + yy + '" x2="' + (W - padR) +
          '" y2="' + yy + '" class="chart-grid"/>' +
          '<text x="' + (padL - 8) + '" y="' + (Number(yy) + 4) +
          '" text-anchor="end" class="chart-tick">' + v + '</text>';
      }
      var midY = padT + innerH / 2;
      svg += '<text x="14" y="' + midY + '" text-anchor="middle" class="chart-axis-label"' +
        ' transform="rotate(-90 14 ' + midY + ')">Tasks</text>';
      var i;
      for (i = 0; i < n; i += 5) {
        svg += '<text x="' + x(i).toFixed(1) + '" y="' + (H - 10) +
          '" text-anchor="middle" class="chart-tick">' +
          escapeHtml(fmtDay(s.ends[i])) + '</text>';
      }
      // Bands: the board's last column is the bottom band (the canonical
      // CFD reading: completed work grows along the bottom).
      var m = s.order.length;
      var lower = [];
      for (var di = 0; di < n; di++) lower.push(0);
      for (var b = m - 1; b >= 0; b--) {
        var upper = lower.map(function (lo, k) { return lo + s.counts[b][k]; });
        var d = 'M' + x(0).toFixed(1) + ',' + y(upper[0]).toFixed(1);
        for (var j = 1; j < n; j++) d += ' L' + x(j).toFixed(1) + ',' + y(upper[j]).toFixed(1);
        for (var k = n - 1; k >= 0; k--) d += ' L' + x(k).toFixed(1) + ',' + y(lower[k]).toFixed(1);
        d += ' Z';
        var color = PALETTE[b % PALETTE.length];
        var peak = 0;
        s.counts[b].forEach(function (c) { if (c > peak) peak = c; });
        svg += '<path d="' + d + '" fill="' + color + '" fill-opacity="0.75" stroke="' + color + '">' +
          '<title>' + escapeHtml(s.order[b]) + ': up to ' + peak + ' tasks</title></path>';
        lower = upper;
      }
      svg += '</svg>';
      host.innerHTML = svg;

      // Legend in board column order.
      var legend = qs('cumulative-flow-legend');
      if (legend) {
        var html = '';
        s.order.forEach(function (name, bi) {
          html += '<span class="cfd-legend-item"><span class="cfd-swatch" style="background:' +
            PALETTE[bi % PALETTE.length] + '"></span>' + escapeHtml(name) + '</span>';
        });
        legend.innerHTML = html;
      }
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      var close = qs('cumulative-flow-close');
      if (close) close.addEventListener('click', function () {
        window.location.href = '/b/' + encodeURIComponent(boardId);
      });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' }
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        var s = seriesByDay(data);
        renderChart(s);
        var total = 0;
        if (s.ends.length) {
          var last = s.ends.length - 1;
          s.counts.forEach(function (c) { total += c[last]; });
        }
        setStatus((data.tasks || []).length === 0
          ? 'No tasks on this board.'
          : (total === 1
            ? '1 task across ' + s.order.length + ' columns over the last 30 days.'
            : total + ' tasks across ' + s.order.length + ' columns over the last 30 days.'));
      }).catch(function (err) {
        setStatus('Could not load task data: ' + err.message);
      });
    }

    return { init: init };
  })();

  // KF-341: Due date performance report page. How well tasks meet their due
  // dates, rendered from the shared export-data dataset.
  // Categories: on time (completed_at <= due_at, or the due date was marked
  // done), late (completed_at > due_at), overdue (due date passed, not
  // completed), upcoming (due date in the future, not completed).
  var DueDatePage = (function () {
    var initialized = false;

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('due-date-status');
      if (el) el.textContent = msg;
    }

    function fmtDateTime(ms) {
      var d = new Date(ms);
      return d.toLocaleDateString('en-US', { day: 'numeric', month: 'short', year: 'numeric' }) +
        ', ' + d.toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit' });
    }

    // Classify each task that has a parseable due date.
    function classify(tasks) {
      var now = Date.now();
      var rows = [];
      tasks.forEach(function (t) {
        if (!t.due_at) return;
        var due = new Date(t.due_at);
        if (isNaN(due.getTime())) return;
        var done = t.completed_at ? new Date(t.completed_at) : null;
        if (done && isNaN(done.getTime())) done = null;
        var status, cls;
        if (done) {
          if (done.getTime() <= due.getTime()) { status = 'On time'; cls = 'report-status-ontime'; }
          else { status = 'Late'; cls = 'report-status-late'; }
        } else if (t.due_done) {
          status = 'On time'; cls = 'report-status-ontime';
        } else if (due.getTime() < now) {
          status = 'Overdue'; cls = 'report-status-overdue';
        } else {
          status = 'Upcoming'; cls = 'report-status-upcoming';
        }
        rows.push({
          name: t.name || '(untitled)',
          dueMs: due.getTime(),
          dueText: fmtDateTime(due.getTime()),
          doneText: done ? fmtDateTime(done.getTime()) : '—',
          status: status,
          cls: cls
        });
      });
      rows.sort(function (a, b) { return a.dueMs - b.dueMs; });
      return rows;
    }

    function render(rows) {
      var host = qs('due-date-summary');
      if (host) {
        var counts = { 'On time': 0, 'Late': 0, 'Overdue': 0, 'Upcoming': 0 };
        rows.forEach(function (r) { counts[r.status]++; });
        function item(label, cls, n) {
          return '<span class="report-summary-item"><strong>' + n + '</strong>' +
            '<span class="' + cls + '">' + label + '</span></span>';
        }
        host.innerHTML =
          item('On time', 'report-status-ontime', counts['On time']) +
          item('Late', 'report-status-late', counts['Late']) +
          item('Overdue', 'report-status-overdue', counts['Overdue']) +
          item('Upcoming', 'report-status-upcoming', counts['Upcoming']);
      }
      var table = qs('due-date-table');
      var tbody = qs('due-date-rows');
      if (table && tbody) {
        if (rows.length === 0) {
          table.hidden = true;
        } else {
          table.hidden = false;
          tbody.innerHTML = rows.map(function (r) {
            return '<tr><td>' + escapeHtml(r.name) + '</td>' +
              '<td>' + escapeHtml(r.dueText) + '</td>' +
              '<td>' + escapeHtml(r.doneText) + '</td>' +
              '<td class="' + r.cls + '">' + r.status + '</td></tr>';
          }).join('');
        }
      }
      var withDue = rows.length;
      var onTime = rows.filter(function (r) { return r.status === 'On time'; }).length;
      setStatus(withDue === 0
        ? 'No tasks with due dates on this board.'
        : onTime + ' of ' + withDue + ' tasks with due dates completed on time.');
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      var close = qs('due-date-close');
      if (close) close.addEventListener('click', function () {
        window.location.href = '/b/' + encodeURIComponent(boardId);
      });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' }
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        render(classify(data.tasks || []));
      }).catch(function (err) {
        setStatus('Could not load task data: ' + err.message);
      });
    }

    return { init: init };
  })();

  // KF-336: Board history report page. A detailed chronological log of
  // changes made on the board (KanbanFlow's Board history: filter the history
  // log by user or type of change; compare a field's value side-by-side
  // before and after the change), rendered from the shared export-data
  // dataset. Comment bodies come from each task's comments array (exact
  // timestamps and text); the duplicate comment_added history events are
  // skipped in favor of those richer entries.
  var BoardHistoryPage = (function () {
    var initialized = false;

    var MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun',
      'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

    // Machine-readable history kinds -> human labels (headlines + filter).
    var KIND_LABELS = {
      created: 'Created',
      moved: 'Column',
      renamed: 'Renamed',
      description_updated: 'Description',
      estimate_updated: 'Time estimate',
      color_updated: 'Color',
      labels_updated: 'Labels',
      due_updated: 'Due date',
      due_done_updated: 'Due date done',
      members_updated: 'Members',
      grouping_date_updated: 'Grouping date',
      subtask_renamed: 'Subtask',
      time_logged: 'Time logged',
      time_edited: 'Time edited',
      time_deleted: 'Time deleted',
      comment: 'Comment'
    };

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('history-status');
      if (el) el.textContent = msg;
    }

    function pad2(n) { return (n < 10 ? '0' : '') + n; }

    // "Today 11:48" / "Yesterday 10:55" / "12. Oct 10:55" (local).
    function fmtTime(ms) {
      var d = new Date(ms);
      var now = new Date();
      var time = pad2(d.getHours()) + ':' + pad2(d.getMinutes());
      var dayStart = new Date(now); dayStart.setHours(0, 0, 0, 0);
      var dStart = new Date(d); dStart.setHours(0, 0, 0, 0);
      var diffDays = Math.round((dayStart - dStart) / 86400000);
      if (diffDays === 0) return 'Today ' + time;
      if (diffDays === 1) return 'Yesterday ' + time;
      return d.getDate() + '. ' + MONTHS[d.getMonth()] + ' ' + time;
    }

    function initials(name) {
      var parts = (name || '').trim().split(/\s+/);
      var s = parts.slice(0, 2).map(function (p) { return p.charAt(0); }).join('');
      return (s || '?').toUpperCase();
    }

    function parseMs(s) {
      var t = new Date(s).getTime();
      return isNaN(t) ? null : t;
    }

    // Flatten every task's history events plus one entry per comment into a
    // single newest-first list. comment_added history events are skipped —
    // the comment entries below carry the same information plus the body.
    function collectEntries(data) {
      var entries = [];
      (data.tasks || []).forEach(function (t) {
        (t.history || []).forEach(function (e) {
          if (e.kind === 'comment_added') return;
          var ms = parseMs(e.created_at);
          if (ms === null) return;
          entries.push({
            kind: e.kind, detail: e.detail, actor: e.actor, ms: ms,
            task: t.name
          });
        });
        (t.comments || []).forEach(function (c) {
          var ms = parseMs(c.created_at);
          if (ms === null) return;
          entries.push({
            kind: 'comment', detail: c.body, actor: c.author, ms: ms,
            task: t.name
          });
        });
      });
      entries.sort(function (a, b) { return b.ms - a.ms; });
      return entries;
    }

    // "Moved from X to Y" -> {from, to} for the side-by-side table.
    function parseMove(detail) {
      var m = /^Moved from (.*) to (.*)$/.exec(detail || '');
      return m ? { from: m[1], to: m[2] } : null;
    }

    function headline(e) {
      var t = '<b>' + escapeHtml(e.task) + '</b>';
      switch (e.kind) {
        case 'created': {
          // detail: "Task created in Backlog"
          var col = (e.detail || '').replace(/^Task created in /, '');
          return 'Task ' + t + ' was created in column <b>' +
            escapeHtml(col) + '</b>';
        }
        case 'moved':
          return 'Task ' + t + ' has changed properties: Column.';
        case 'renamed':
          return 'Task ' + t + ' was renamed.';
        case 'comment':
          return '<b>' + escapeHtml(e.actor) + '</b> added to task ' + t +
            ' with text';
        case 'time_logged':
        case 'time_edited':
        case 'time_deleted': {
          var d = e.detail || '';
          return 'Task ' + t + ': ' + escapeHtml(d.charAt(0).toLowerCase() + d.slice(1));
        }
        default: {
          var label = KIND_LABELS[e.kind] || e.kind;
          return 'Task ' + t + ' has changed properties: ' +
            escapeHtml(label) + '.';
        }
      }
    }

    function renderEntry(e) {
      var html = '<div class="history-entry">';
      html += '<div class="history-head">';
      html += '<span class="user-avatar history-avatar" aria-hidden="true">' +
        escapeHtml(initials(e.actor)) + '</span>';
      html += '<span class="history-actor">' + escapeHtml(e.actor) + '</span>';
      html += '<span class="history-time">' + escapeHtml(fmtTime(e.ms)) + '</span>';
      html += '</div>';
      html += '<div class="history-text">' + headline(e) + '</div>';
      if (e.kind === 'moved') {
        var mv = parseMove(e.detail);
        if (mv) {
          html += '<details class="history-compare" open>' +
            '<summary>Compare values</summary>' +
            '<table><thead><tr><th>Property</th><th>Old value</th><th>New value</th></tr></thead>' +
            '<tbody><tr><th>Column</th><td>' + escapeHtml(mv.from) + '</td><td>' +
            escapeHtml(mv.to) + '</td></tr></tbody></table></details>';
        }
      } else if (e.kind === 'comment') {
        html += '<div class="history-comment">' + escapeHtml(e.detail) + '</div>';
      } else if (e.kind === 'renamed') {
        // detail: Renamed to "X"
        html += '<div class="history-comment">' + escapeHtml(e.detail) + '</div>';
      }
      html += '</div>';
      return html;
    }

    function fillFilters(entries) {
      var userSel = qs('history-user-filter');
      var typeSel = qs('history-type-filter');
      var users = {}, kinds = {};
      entries.forEach(function (e) {
        users[e.actor] = true;
        kinds[e.kind] = true;
      });
      Object.keys(users).sort().forEach(function (u) {
        var o = document.createElement('option');
        o.value = u; o.textContent = u;
        userSel.appendChild(o);
      });
      Object.keys(kinds).sort(function (a, b) {
        var la = KIND_LABELS[a] || a, lb = KIND_LABELS[b] || b;
        return la < lb ? -1 : la > lb ? 1 : 0;
      }).forEach(function (k) {
        var o = document.createElement('option');
        o.value = k; o.textContent = KIND_LABELS[k] || k;
        typeSel.appendChild(o);
      });
    }

    function render(entries) {
      var userF = qs('history-user-filter').value;
      var typeF = qs('history-type-filter').value;
      var shown = entries.filter(function (e) {
        return (!userF || e.actor === userF) && (!typeF || e.kind === typeF);
      });
      var feed = qs('history-feed');
      if (!shown.length) {
        feed.innerHTML = '';
        setStatus(entries.length
          ? 'No changes match the selected filters.'
          : 'No changes on this board.');
        return;
      }
      feed.innerHTML = shown.map(renderEntry).join('');
      setStatus(shown.length === 1 ? '1 change.' : shown.length + ' changes.');
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      var close = qs('history-close');
      if (close) close.addEventListener('click', function () {
        window.location.href = '/b/' + encodeURIComponent(boardId);
      });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' }
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        var entries = collectEntries(data);
        fillFilters(entries);
        qs('history-user-filter').addEventListener('change', function () { render(entries); });
        qs('history-type-filter').addEventListener('change', function () { render(entries); });
        render(entries);
      }).catch(function (err) {
        setStatus('Could not load board history: ' + err.message);
      });
    }

    return { init: init };
  })();

  // KF-338: Calendar report page. Tasks laid out by due date on a month grid
  // (KanbanFlow's Calendar: "a better overview of when your tasks are due",
  // filterable by user, color, labels and swimlane), rendered from the
  // shared export-data dataset. Monday-start weeks, like the other reports.
  var CalendarPage = (function () {
    var initialized = false;

    var MONTHS = ['January', 'February', 'March', 'April', 'May', 'June',
      'July', 'August', 'September', 'October', 'November', 'December'];
    var MONTHS_SHORT = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun',
      'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
    var WEEKDAYS = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];

    var viewYear, viewMonth; // month: 0-based
    var dataset = null;

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('calendar-status');
      if (el) el.textContent = msg;
    }

    function pad2(n) { return (n < 10 ? '0' : '') + n; }

    function dayKey(d) {
      return d.getFullYear() + '-' + pad2(d.getMonth() + 1) + '-' + pad2(d.getDate());
    }

    function parseLocalDay(s) {
      var t = new Date(s).getTime();
      if (isNaN(t)) return null;
      var d = new Date(t);
      return new Date(d.getFullYear(), d.getMonth(), d.getDate());
    }

    // KF-352: expand due_repeat into recurrence instances. Mirrors the
    // server's parse_repeat_cadence ("every week" from the dialog checkbox,
    // plus plain-English alternates); unrecognized text -> no expansion.
    function parseRepeatCadence(text) {
      var s = (text || '').trim().toLowerCase();
      if (s === 'every day' || s === 'daily') return 'daily';
      if (s === 'every week' || s === 'weekly') return 'weekly';
      if (s === 'every month' || s === 'monthly') return 'monthly';
      if (s === 'every year' || s === 'yearly' || s === 'annually') return 'yearly';
      return null;
    }

    function addMonthsClamped(d, n) {
      var day = d.getDate();
      var r = new Date(d.getFullYear(), d.getMonth() + n, 1,
        d.getHours(), d.getMinutes(), d.getSeconds(), d.getMilliseconds());
      var last = new Date(r.getFullYear(), r.getMonth() + 1, 0).getDate();
      r.setDate(Math.min(day, last));
      return r;
    }

    function stepRepeat(d, cadence) {
      if (cadence === 'daily') { var a = new Date(d.getTime()); a.setDate(a.getDate() + 1); return a; }
      if (cadence === 'weekly') { var w = new Date(d.getTime()); w.setDate(w.getDate() + 7); return w; }
      if (cadence === 'monthly') return addMonthsClamped(d, 1);
      if (cadence === 'yearly') { var y = new Date(d.getTime()); y.setFullYear(y.getFullYear() + 1); return y; }
      return new Date(d.getTime());
    }

    // Recurrence instances of a due day within [start, end], exclusive of
    // the original day (rendered separately). Capped so pathological input
    // can't spin forever.
    function repeatInstances(dueDay, repeatText, start, end) {
      var cadence = parseRepeatCadence(repeatText);
      if (!cadence) return [];
      var out = [];
      var d = new Date(dueDay.getTime());
      for (var i = 0; i < 400; i++) {
        d = stepRepeat(d, cadence);
        if (d > end) break;
        if (d >= start) out.push(new Date(d.getTime()));
      }
      return out;
    }


    // "On today" / "On tomorrow" / "Yesterday" / "On 12. Oct" (local).
    function fmtRelative(day) {
      var now = new Date();
      var today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
      var diff = Math.round((day - today) / 86400000);
      if (diff === 0) return 'On today';
      if (diff === 1) return 'On tomorrow';
      if (diff === -1) return 'Yesterday';
      return 'On ' + day.getDate() + '. ' + MONTHS_SHORT[day.getMonth()];
    }

    // Pick readable text for a chip background hex color.
    function chipText(hex) {
      var m = /^#?([0-9a-f]{6})$/i.exec(hex || '');
      if (!m) return '#111827';
      var r = parseInt(m[1].slice(0, 2), 16);
      var g = parseInt(m[1].slice(2, 4), 16);
      var b = parseInt(m[1].slice(4, 6), 16);
      var lum = (0.299 * r + 0.587 * g + 0.114 * b) / 255;
      return lum > 0.6 ? '#111827' : '#ffffff';
    }

    function colNameById(id) {
      var cols = (dataset && dataset.columns) || [];
      for (var i = 0; i < cols.length; i++) {
        if (cols[i].id === id) return cols[i].name;
      }
      return '';
    }

    function colorValueById(id) {
      var colors = (dataset && dataset.colors) || [];
      for (var i = 0; i < colors.length; i++) {
        if (colors[i].id === id) return colors[i].value;
      }
      return '';
    }

    function passesFilters(t) {
      var uf = qs('calendar-user-filter').value;
      var cf = qs('calendar-color-filter').value;
      var lf = qs('calendar-label-filter').value;
      var sf = qs('calendar-swimlane-filter').value;
      if (uf && (t.member_ids || []).indexOf(uf) < 0) return false;
      if (cf && (t.color_id || '') !== cf) return false;
      if (lf && (t.labels || []).indexOf(lf) < 0) return false;
      if (sf && (t.swimlane_id || '') !== sf) return false;
      return true;
    }

    function renderChip(t, day) {
      var bg = colorValueById(t.color_id) || '#e5e7eb';
      var fg = chipText(bg);
      var sub = fmtRelative(day) + ' \u2192 ' + colNameById(t.column_id);
      return '<div class="cal-chip" style="background:' + escapeHtml(bg) +
        ';color:' + fg + '" title="' + escapeHtml(t.name) + '">' +
        '<div class="cal-chip-name">' + escapeHtml(t.name) + '</div>' +
        '<div class="cal-chip-sub">' + escapeHtml(sub) + '</div></div>';
    }

    function render() {
      var grid = qs('calendar-grid');
      qs('calendar-title').textContent = MONTHS[viewMonth] + ' ' + viewYear;

      // Monday-start: find the Monday of the week containing the 1st, and
      // the Sunday of the week containing the month's last day.
      var first = new Date(viewYear, viewMonth, 1);
      var start = new Date(first);
      start.setDate(start.getDate() - ((start.getDay() + 6) % 7));
      var last = new Date(viewYear, viewMonth + 1, 0);
      var end = new Date(last);
      end.setDate(end.getDate() + (6 - ((end.getDay() + 6) % 7)));

      // Bucket visible tasks by local due day.
      var byDay = {};
      var shown = 0;
      function addChip(t, day) {
        if (day < start || day > end) return;
        var k = dayKey(day);
        (byDay[k] = byDay[k] || []).push({ t: t, day: day });
        shown++;
      }
      (dataset.tasks || []).forEach(function (t) {
        if (!t.due_at || !passesFilters(t)) return;
        var day = parseLocalDay(t.due_at);
        if (!day) return;
        addChip(t, day);
        // KF-352: expand recognized due_repeat cadences into recurrence
        // instances within the visible range.
        var instances = repeatInstances(day, t.due_repeat, start, end);
        for (var i = 0; i < instances.length; i++) addChip(t, instances[i]);
      });

      var todayK = dayKey(new Date());
      var html = '';
      WEEKDAYS.forEach(function (w) {
        html += '<div class="cal-dow" role="columnheader">' + w + '</div>';
      });
      for (var d = new Date(start); d <= end; d.setDate(d.getDate() + 1)) {
        var k = dayKey(d);
        var cls = 'cal-day';
        if (d.getMonth() !== viewMonth) cls += ' cal-outside';
        if (k === todayK) cls += ' cal-today';
        html += '<div class="' + cls + '" role="gridcell">';
        html += '<div class="cal-daynum">' + d.getDate() + '</div>';
        (byDay[k] || []).forEach(function (item) {
          html += renderChip(item.t, item.day);
        });
        html += '</div>';
      }
      grid.innerHTML = html;
      setStatus(shown === 0
        ? 'No tasks with due dates in view.'
        : (shown === 1 ? '1 task with a due date in view.'
          : shown + ' tasks with due dates in view.'));
    }

    function fillFilters() {
      var users = {}, colors = {}, labels = {}, swimlanes = {};
      (dataset.tasks || []).forEach(function (t) {
        (t.member_ids || []).forEach(function (id) { users[id] = true; });
        if (t.color_id) colors[t.color_id] = true;
        (t.labels || []).forEach(function (l) { labels[l] = true; });
        if (t.swimlane_id) swimlanes[t.swimlane_id] = true;
      });
      function fill(id, map, labelById) {
        var sel = qs(id);
        Object.keys(map).sort().forEach(function (key) {
          var o = document.createElement('option');
          o.value = key;
          o.textContent = labelById(key) || key;
          sel.appendChild(o);
        });
      }
      var userName = function (id) {
        var us = dataset.users || [];
        for (var i = 0; i < us.length; i++) if (us[i].id === id) return us[i].username;
        return id;
      };
      var colorLabel = function (id) {
        var cs = dataset.colors || [];
        for (var i = 0; i < cs.length; i++) if (cs[i].id === id) return cs[i].label || cs[i].value;
        return id;
      };
      var namedLabel = function (list) {
        return function (id) {
          for (var i = 0; i < list.length; i++) if (list[i].id === id) return list[i].name;
          return id;
        };
      };
      fill('calendar-user-filter', users, userName);
      fill('calendar-color-filter', colors, colorLabel);
      fill('calendar-label-filter', labels, function (l) { return l; });
      fill('calendar-swimlane-filter', swimlanes, namedLabel(dataset.swimlanes || []));
      ['calendar-user-filter', 'calendar-color-filter',
        'calendar-label-filter', 'calendar-swimlane-filter'].forEach(function (id) {
        qs(id).addEventListener('change', render);
      });
    }

    function feedStatus(msg) {
      var el = qs('calendar-feed-status');
      if (el) el.textContent = msg;
    }

    // KF-351: iCal feed subscription. The bearer token is fetched via the
    // authenticated link endpoint only when the user opens the panel — it
    // is never embedded in the page source.
    function initFeed(boardId) {
      var toggle = qs('calendar-feed-toggle');
      var panel = qs('calendar-feed-panel');
      if (!toggle || !panel) return;
      var linkPath = null;
      function urls() {
        var https = window.location.origin + linkPath;
        return { https: https, webcal: https.replace(/^https?:\/\//, 'webcal://') };
      }
      function refreshLinks() {
        var sub = qs('calendar-feed-subscribe');
        if (sub) sub.href = urls().webcal;
      }
      toggle.addEventListener('click', function () {
        var opening = panel.hidden;
        panel.hidden = !panel.hidden;
        if (!opening || linkPath) return;
        feedStatus('Loading feed link…');
        fetch('/api/boards/' + encodeURIComponent(boardId) + '/calendar-feed', {
          headers: { 'Accept': 'application/json' }
        }).then(function (r) {
          if (!r.ok) throw new Error('HTTP ' + r.status);
          return r.json();
        }).then(function (data) {
          linkPath = data.path;
          refreshLinks();
          feedStatus('');
        }).catch(function (err) {
          feedStatus('Could not load feed link: ' + err.message);
        });
      });
      var copy = qs('calendar-feed-copy');
      if (copy) copy.addEventListener('click', function () {
        if (!linkPath) return;
        var u = urls().https;
        function done() { feedStatus('Feed link copied.'); }
        if (navigator.clipboard && navigator.clipboard.writeText) {
          navigator.clipboard.writeText(u).then(done, function () { feedStatus(u); });
        } else {
          feedStatus(u);
        }
      });
      var reset = qs('calendar-feed-reset');
      if (reset) reset.addEventListener('click', function () {
        if (!window.confirm('Reset the iCal feed link? The old link will stop working.')) return;
        feedStatus('Resetting…');
        fetch('/api/boards/' + encodeURIComponent(boardId) + '/calendar-feed/reset', {
          method: 'POST',
          headers: { 'Accept': 'application/json' }
        }).then(function (r) {
          if (!r.ok) throw new Error('HTTP ' + r.status);
          return r.json();
        }).then(function (data) {
          linkPath = data.path;
          refreshLinks();
          feedStatus('New feed link issued; the old one no longer works.');
        }).catch(function (err) {
          feedStatus('Could not reset feed link: ' + err.message);
        });
      });
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      var now = new Date();
      viewYear = now.getFullYear();
      viewMonth = now.getMonth();
      var close = qs('calendar-close');
      if (close) close.addEventListener('click', function () {
        window.location.href = '/b/' + encodeURIComponent(boardId);
      });
      initFeed(boardId);
      qs('calendar-prev').addEventListener('click', function () {
        viewMonth--;
        if (viewMonth < 0) { viewMonth = 11; viewYear--; }
        render();
      });
      qs('calendar-next').addEventListener('click', function () {
        viewMonth++;
        if (viewMonth > 11) { viewMonth = 0; viewYear++; }
        render();
      });
      qs('calendar-today').addEventListener('click', function () {
        var t = new Date();
        viewYear = t.getFullYear();
        viewMonth = t.getMonth();
        render();
      });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' }
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        dataset = data;
        fillFilters();
        render();
      }).catch(function (err) {
        setStatus('Could not load calendar data: ' + err.message);
      });
    }

    return { init: init };
  })();

  // KF-342: Monte Carlo forecasting report page — predicts when N tasks will
  // be completed by running thousands of simulations sampled from the
  // board's historical daily throughput (arrivals into the target column,
  // from "moved" history events). Renders client-side from the shared
  // export-data dataset; pure inline SVG, no chart library.
  var MonteCarloPage = (function () {
    var initialized = false;

    var MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun',
      'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
    var FULL_MONTHS = ['January', 'February', 'March', 'April', 'May', 'June',
      'July', 'August', 'September', 'October', 'November', 'December'];
    // KanbanFlow's forecast percentiles, with the reference screenshot's
    // line colors.
    var PCTS = [95, 85, 75, 50];
    var PCT_COLORS = { 95: '#43a047', 85: '#c49000', 75: '#c0392b', 50: '#8e44ad' };

    var lastData = null;   // export-data payload
    var lastBasis = null;  // { starts, counts } for the current settings

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('mc-status');
      if (el) el.textContent = msg;
    }

    function fmtDay(d) {
      return d.getDate() + '. ' + MONTHS[d.getMonth()];
    }

    function fmtForecastDate(ms) {
      var d = new Date(ms);
      return FULL_MONTHS[d.getMonth()] + ' ' + d.getDate() + ', ' + d.getFullYear();
    }

    function todayLocal() {
      var d = new Date();
      return new Date(d.getFullYear(), d.getMonth(), d.getDate());
    }

    function isoDate(d) {
      var m = d.getMonth() + 1, day = d.getDate();
      return d.getFullYear() + '-' + (m < 10 ? '0' : '') + m + '-' + (day < 10 ? '0' : '') + day;
    }

    // "Moved from {from} to {to}" — split on the last " to " so column
    // names containing " to " parse deterministically (same approach as
    // CumulativeFlowPage).
    function parseMoveDetail(detail) {
      var body = detail.slice('Moved from '.length);
      var i = body.lastIndexOf(' to ');
      if (i === -1) return null;
      return { from: body.slice(0, i), to: body.slice(i + 4) };
    }

    // Per-day arrival counts into the target column over the last `days`
    // days (oldest first). Arrivals come from "moved" history events whose
    // destination matches the target column's current name.
    function dailyThroughput(tasks, targetName, days) {
      var start = todayLocal();
      start.setDate(start.getDate() - (days - 1));
      var startMs = start.getTime();
      var counts = [];
      for (var i = 0; i < days; i++) counts.push(0);
      tasks.forEach(function (t) {
        (t.history || []).forEach(function (e) {
          if (!e || e.kind !== 'moved' || e.detail.indexOf('Moved from ') !== 0) return;
          var m = parseMoveDetail(e.detail);
          if (!m || m.to !== targetName) return;
          var d = new Date(e.created_at);
          if (isNaN(d.getTime())) return;
          var day = new Date(d.getFullYear(), d.getMonth(), d.getDate());
          var idx = Math.round((day.getTime() - startMs) / 86400000);
          if (idx >= 0 && idx < days) counts[idx]++;
        });
      });
      var starts = [];
      for (var j = 0; j < days; j++) starts.push(new Date(startMs + j * 86400000));
      return { starts: starts, counts: counts };
    }

    // Run `sims` simulations: each simulated day samples one historical
    // day's throughput (uniformly, with replacement) until `nTasks` are
    // done. Returns the sorted completion timestamps, or null when there is
    // no historical throughput to sample from.
    function runSimulation(counts, nTasks, sims, startMs) {
      var total = 0, k;
      for (k = 0; k < counts.length; k++) total += counts[k];
      if (total === 0) return null;
      var out = [];
      var cap = 365 * 5; // give up after five simulated years
      for (var s = 0; s < sims; s++) {
        var done = 0, d = 0;
        while (done < nTasks && d < cap) {
          done += counts[(Math.random() * counts.length) | 0];
          d++;
        }
        out.push(startMs + d * 86400000);
      }
      out.sort(function (a, b) { return a - b; });
      return out;
    }

    function percentile(sorted, p) {
      var i = Math.ceil(p / 100 * sorted.length) - 1;
      if (i < 0) i = 0;
      if (i >= sorted.length) i = sorted.length - 1;
      return sorted[i];
    }

    function renderForecast(nTasks, startMs, sorted) {
      var head = qs('mc-headline');
      if (head) head.textContent = 'Forecast to complete ' + nTasks +
        (nTasks === 1 ? ' task' : ' tasks') + ' starting ' + fmtDay(new Date(startMs));
      var lines = qs('mc-forecast-lines');
      if (lines) {
        var html = '';
        PCTS.forEach(function (p) {
          var at = percentile(sorted, p);
          html += '<div class="mc-forecast-line" style="color:' + PCT_COLORS[p] + '">' +
            escapeHtml(fmtForecastDate(at)) + ' (' + p + '% probability)</div>';
        });
        lines.innerHTML = html;
      }
      renderChart(sorted);
    }

    // Histogram of simulated completion dates (one bar per day) with dashed
    // vertical percentile lines, KanbanFlow-style.
    function renderChart(sorted) {
      var host = qs('mc-chart');
      if (!host) return;
      var DAY = 86400000;
      var minD = Math.floor(sorted[0] / DAY) * DAY;
      var maxD = Math.floor(sorted[sorted.length - 1] / DAY) * DAY;
      var nDays = Math.max(1, Math.round((maxD - minD) / DAY) + 1);
      var buckets = [];
      for (var i = 0; i < nDays; i++) buckets.push(0);
      sorted.forEach(function (ms) {
        var idx = Math.round((Math.floor(ms / DAY) * DAY - minD) / DAY);
        if (idx >= 0 && idx < nDays) buckets[idx]++;
      });
      var max = 1;
      buckets.forEach(function (b) { if (b > max) max = b; });

      var W = 720, H = 300, padL = 52, padR = 24, padT = 26, padB = 34;
      var innerW = W - padL - padR, innerH = H - padT - padB;
      var slot = innerW / nDays;
      var bw = Math.max(1, Math.min(slot * 0.8, 24));
      function x(bi) { return padL + slot * bi + (slot - bw) / 2; }
      function y(v) { return padT + innerH - (v / max) * innerH; }

      var svg = '<svg viewBox="0 0 ' + W + ' ' + H + '" class="line-chart">';
      var t, v, yy, b, dIdx;
      for (t = 0; t <= 4; t++) {
        v = Math.round(max * t / 4);
        yy = y(v).toFixed(1);
        svg += '<line x1="' + padL + '" y1="' + yy + '" x2="' + (W - padR) +
          '" y2="' + yy + '" class="chart-grid"/>' +
          '<text x="' + (padL - 8) + '" y="' + (Number(yy) + 4) +
          '" text-anchor="end" class="chart-tick">' + v + '</text>';
      }
      // Month ticks: label at most ~8 evenly spaced month starts so the
      // labels never crowd (forecasts can span years).
      var monthIdx = [];
      for (dIdx = 0; dIdx < nDays; dIdx++) {
        var mdt = new Date(minD + dIdx * DAY);
        if (mdt.getDate() === 1) monthIdx.push(dIdx);
      }
      var mStep = Math.max(1, Math.ceil(monthIdx.length / 8));
      monthIdx.forEach(function (mi, mk) {
        if (mk % mStep !== 0) return;
        var mdt2 = new Date(minD + mi * DAY);
        svg += '<text x="' + (padL + slot * mi + slot / 2).toFixed(1) + '" y="' + (H - 10) +
          '" text-anchor="middle" class="chart-tick">' + escapeHtml(fmtDay(mdt2)) + '</text>';
      });
      for (b = 0; b < nDays; b++) {
        if (!buckets[b]) continue;
        var bh = (buckets[b] / max) * innerH;
        svg += '<rect x="' + x(b).toFixed(1) + '" y="' + (padT + innerH - bh).toFixed(1) +
          '" width="' + bw.toFixed(1) + '" height="' + bh.toFixed(1) +
          '" class="mc-bar"><title>' + buckets[b] +
          (buckets[b] === 1 ? ' simulation' : ' simulations') +
          ' complete ' + escapeHtml(fmtDay(new Date(minD + b * DAY))) + '</title></rect>';
      }
      // Percentile markers.
      PCTS.forEach(function (p) {
        var at = percentile(sorted, p);
        var pos = (Math.floor(at / DAY) * DAY - minD) / DAY;
        var px = padL + slot * pos + slot / 2;
        svg += '<line x1="' + px.toFixed(1) + '" y1="' + padT + '" x2="' + px.toFixed(1) +
          '" y2="' + (padT + innerH) + '" class="mc-pct" style="stroke:' + PCT_COLORS[p] + '"/>' +
          '<text x="' + px.toFixed(1) + '" y="' + (padT - 8) + '" text-anchor="middle" class="chart-tick" ' +
          'style="fill:' + PCT_COLORS[p] + '">' + p + '%</text>';
      });
      svg += '</svg>';
      host.innerHTML = svg;
    }

    // Throughput-basis line chart: historical completions per day.
    function renderBasis(basis) {
      var host = qs('mc-basis');
      if (!host) return;
      var counts = basis.counts, starts = basis.starts;
      var W = 720, H = 220, padL = 40, padR = 24, padT = 14, padB = 34;
      var innerW = W - padL - padR, innerH = H - padT - padB;
      var max = 1;
      counts.forEach(function (c) { if (c > max) max = c; });
      var n = counts.length;
      function x(i) { return padL + (n <= 1 ? innerW / 2 : (i / (n - 1)) * innerW); }
      function y(v) { return padT + innerH - (v / max) * innerH; }
      var svg = '<svg viewBox="0 0 ' + W + ' ' + H + '" class="line-chart">';
      var t, v, yy, i, j;
      var steps = Math.min(4, max);
      for (t = 0; t <= steps; t++) {
        v = Math.round(max * t / steps);
        yy = y(v).toFixed(1);
        svg += '<line x1="' + padL + '" y1="' + yy + '" x2="' + (W - padR) +
          '" y2="' + yy + '" class="chart-grid"/>' +
          '<text x="' + (padL - 8) + '" y="' + (Number(yy) + 4) +
          '" text-anchor="end" class="chart-tick">' + v + '</text>';
      }
      for (i = 0; i < n; i++) {
        if (starts[i].getDate() === 1) {
          svg += '<text x="' + x(i).toFixed(1) + '" y="' + (H - 10) +
            '" text-anchor="middle" class="chart-tick">' + escapeHtml(fmtDay(starts[i])) + '</text>';
        }
      }
      var pts = [];
      for (j = 0; j < n; j++) pts.push(x(j).toFixed(1) + ',' + y(counts[j]).toFixed(1));
      svg += '<polyline points="' + pts.join(' ') + '" class="chart-line" fill="none"/>';
      svg += '</svg>';
      host.innerHTML = svg;
    }

    function targetColumnName() {
      var sel = qs('mc-target-column');
      if (!sel || !sel.options.length) return '';
      return sel.options[sel.selectedIndex].text;
    }

    function refreshBasis() {
      if (!lastData) return;
      var days = parseInt(qs('mc-period').value, 10) || 90;
      lastBasis = dailyThroughput(lastData.tasks || [], targetColumnName(), days);
      renderBasis(lastBasis);
    }

    function runForecast() {
      if (!lastData) return;
      var nTasks = parseInt(qs('mc-task-count').value, 10);
      if (isNaN(nTasks) || nTasks < 1) nTasks = 1;
      qs('mc-task-count').value = nTasks;
      var sims = parseInt(qs('mc-sims').value, 10);
      if (isNaN(sims) || sims < 100) sims = 100;
      if (sims > 10000) sims = 10000;
      qs('mc-sims').value = sims;
      var startVal = qs('mc-start-date').value;
      var startMs;
      if (startVal) {
        var parts = startVal.split('-');
        startMs = new Date(+parts[0], +parts[1] - 1, +parts[2]).getTime();
      } else {
        startMs = todayLocal().getTime();
      }
      refreshBasis();
      var sorted = runSimulation(lastBasis.counts, nTasks, sims, startMs);
      if (!sorted) {
        qs('mc-headline').textContent = '';
        qs('mc-forecast-lines').innerHTML = '';
        qs('mc-chart').innerHTML = '';
        setStatus('No completed tasks in the selected period — the forecast needs historical throughput.');
        return;
      }
      renderForecast(nTasks, startMs, sorted);
      var total = lastBasis.counts.reduce(function (a, b) { return a + b; }, 0);
      setStatus('Forecast based on ' + total + ' completed tasks in the selected period (' + sims + ' simulations).');
    }

    function init(boardId, doneColumnId) {
      if (initialized) return;
      initialized = true;
      var close = qs('mc-close');
      if (close) close.addEventListener('click', function () {
        window.location.href = '/b/' + encodeURIComponent(boardId);
      });
      qs('mc-start-date').value = isoDate(todayLocal());
      qs('mc-run').addEventListener('click', runForecast);
      qs('mc-period').addEventListener('change', refreshBasis);
      qs('mc-target-column').addEventListener('change', refreshBasis);
      var more = qs('mc-more'), simsWrap = qs('mc-sims-wrap');
      more.addEventListener('change', function () {
        simsWrap.hidden = !more.checked;
      });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' }
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        lastData = data;
        var sel = qs('mc-target-column');
        var cols = data.columns || [];
        cols.forEach(function (c) {
          var opt = document.createElement('option');
          opt.value = c.id;
          opt.textContent = c.name;
          sel.appendChild(opt);
        });
        var defId = doneColumnId || (cols.length ? cols[cols.length - 1].id : '');
        if (defId) sel.value = defId;
        refreshBasis();
      }).catch(function (err) {
        setStatus('Could not load forecast data: ' + err.message);
      });
    }

    return { init: init };
  })();

  // KF-345: Time estimate report page — estimated workload by color and
  // column, summed from each task's time estimate (KanbanFlow's Time
  // estimate report: "an overview of the estimated workload on your
  // board"). Renders client-side from the shared export-data dataset.
  var TimeEstimatePage = (function () {
    var initialized = false;

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('time-estimate-status');
      if (el) el.textContent = msg;
    }

    // "23h 00m" — KanbanFlow's Time estimate report cell format.
    function fmtEstimate(hours) {
      var totalMin = Math.round(hours * 60);
      var h = Math.floor(totalMin / 60), m = totalMin % 60;
      return h + 'h ' + (m < 10 ? '0' : '') + m + 'm';
    }

    // Returns { columns, rows } where rows are per color (plus a "No color"
    // row when needed) with per-column summed estimate hours. Mirrors
    // TaskCountPage's grouping; colors with no tasks are omitted and row
    // order follows first appearance on the board.
    function buildTable(data) {
      var columns = data.columns || [];
      var colors = data.colors || [];
      var tasks = data.tasks || [];
      var colorById = {};
      colors.forEach(function (c) { colorById[c.id] = c; });
      var colIndex = {};
      columns.forEach(function (c, i) { colIndex[c.id] = i; });

      var sums = {};
      var order = [];
      var nEstimated = 0;
      tasks.forEach(function (t) {
        var key = t.color_id || '';
        if (order.indexOf(key) === -1) order.push(key);
        if (!sums[key]) sums[key] = columns.map(function () { return 0; });
        var i = colIndex[t.column_id];
        if (i === undefined) return;
        var est = parseFloat(t.estimate_hours);
        if (!isNaN(est) && est > 0) {
          sums[key][i] += est;
          nEstimated++;
        }
      });

      var rows = order.map(function (key) {
        var c = colorById[key];
        return {
          label: c ? (c.label || c.value) : 'No color',
          value: c ? c.value : null,
          sums: sums[key]
        };
      });
      return { columns: columns, rows: rows, nEstimated: nEstimated };
    }

    function render(table) {
      var host = qs('time-estimate-table');
      if (!host) return;
      if (!table.rows.length) {
        host.innerHTML = '<p class="log-status">No tasks on this board.</p>';
        return;
      }
      var html = '<table class="report-table"><thead><tr><th scope="col"><span class="sr-only">Color</span></th>';
      table.columns.forEach(function (c) {
        html += '<th scope="col">' + escapeHtml(c.name) + '</th>';
      });
      html += '</tr></thead><tbody>';
      table.rows.forEach(function (r) {
        html += '<tr><th scope="row"><span class="report-color-dot"' +
          (r.value
            ? ' style="background:' + escapeHtml(r.value) + '"'
            : ' style="background:transparent;border:1px dashed #9ca3af"') +
          '></span> ' + escapeHtml(r.label) + '</th>';
        r.sums.forEach(function (h) {
          html += '<td>' + escapeHtml(fmtEstimate(h)) + '</td>';
        });
        html += '</tr>';
      });
      html += '</tbody></table>';
      host.innerHTML = html;
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      var close = qs('time-estimate-close');
      if (close) close.addEventListener('click', function () {
        window.location.href = '/b/' + encodeURIComponent(boardId);
      });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' }
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        var table = buildTable(data);
        render(table);
        var total = 0;
        table.rows.forEach(function (r) {
          r.sums.forEach(function (h) { total += h; });
        });
        setStatus(table.nEstimated === 0
          ? 'No time estimates on this board.'
          : 'Total estimated workload: ' + fmtEstimate(total) + ' across ' +
            table.nEstimated + (table.nEstimated === 1 ? ' task' : ' tasks') + ' with estimates.');
      }).catch(function (err) {
        setStatus('Could not load time estimate data: ' + err.message);
      });
    }

    return { init: init };
  })();

  function renderBarChart(el, bars) {


    var W = 720, H = 280, padL = 44, padB = 30, padT = 14;
    var max = 1;
    bars.forEach(function (b) { if (b.count > max) max = b.count; });
    var innerW = W - padL - 10;
    var innerH = H - padT - padB;
    var slot = innerW / Math.max(1, bars.length);
    var bw = Math.min(40, slot * 0.55);
    var html = '';
    // Gridlines + y labels (pomodori counts).
    for (var g = 0; g <= 4; g++) {
      var val = Math.round(max * g / 4);
      var y = padT + innerH - (innerH * g / 4);
      html += '<line x1="' + padL + '" y1="' + y + '" x2="' + W + '" y2="' + y +
        '" stroke="#e3e3e3"/>' +
        '<text x="' + (padL - 6) + '" y="' + (y + 4) + '" text-anchor="end" font-size="11" fill="#777">' +
        val + '</text>';
    }
    bars.forEach(function (b, i) {
      var h = innerH * (b.count / max);
      var x = padL + slot * i + (slot - bw) / 2;
      var y = padT + innerH - h;
      var cx = (padL + slot * i + slot / 2).toFixed(1);
      var tip = b.tooltip || (b.label + ': ' + b.count + ' Pomodori (' +
        fmtDuration(b.minutes || 0) + ')');
      html += '<rect x="' + x.toFixed(1) + '" y="' + y.toFixed(1) + '" width="' + bw.toFixed(1) +
        '" height="' + h.toFixed(1) + '" fill="#2f7cf6" rx="2">' +
        '<title>' + escapeHtml(tip) + '</title></rect>' +
        // KF-293: pomodoro count label atop each bar (KanbanFlow).
        '<text x="' + cx + '" y="' + (y - 4).toFixed(1) +
        '" text-anchor="middle" font-size="11" fill="#777">' + b.count + '</text>' +
        '<text x="' + cx + '" y="' + (H - 10) +
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
          // KF-353: sync the toggle switches with live state every time the
          // menu opens (dark/large-names come from localStorage prefs; the
          // legend item is server-rendered but re-sync defensively).
          var darkItem = menu.querySelector('[data-bm="dark"]');
          if (darkItem) darkItem.setAttribute('aria-checked', document.body.classList.contains('dark') ? 'true' : 'false');
          var namesItem = menu.querySelector('[data-bm="large-names"]');
          if (namesItem) namesItem.setAttribute('aria-checked', document.body.classList.contains('large-names') ? 'true' : 'false');
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
          // KF-154: the board menu's Settings opens this board's settings,
          // not the account-level /settings page.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/settings';
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
          this.toggleLegend();
          break;
        case 'save-template':
          // KF-163: KanbanFlow parity — the board menu exposes save-as-template.
          openSaveTemplateDialog();
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
        case 'export':
          // KF-346: Export report page — download the board's tasks.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/export';
          break;
        case 'count':
          // KF-343: Task count report page — tasks per column by color.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/task-count';
          break;
        case 'throughput':
          // KF-344: Throughput report page — completions per week chart.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/throughput';
          break;
        case 'burndown':
          // KF-337: Burndown report page — tasks remaining per day chart.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/burndown';
          break;
        case 'flow':
          // KF-339: Cumulative flow report page — tasks per column over time.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/cumulative-flow';
          break;
        case 'cycle':
          // KF-340: Cycle & lead time report page — avg time per week chart.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/cycle-lead';
          break;
        case 'due':
          // KF-341: Due date performance report page — on-time/late/overdue table.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/due-date';
          break;
        case 'history':
          // KF-336: Board history report page — chronological change log.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/board-history';
          break;
        case 'calendar':
          // KF-338: Calendar report page — tasks by due date on a month grid.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/calendar';
          break;
        case 'montecarlo':
          // KF-342: Monte Carlo forecasting report page — probabilistic completion forecast.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/monte-carlo';
          break;
        case 'estimate':
          // KF-345: Time estimate report page — estimated workload by color and column.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/time-estimate';
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
      this.loadFilterLabels();
      btn.addEventListener('click', function (e) {
        e.stopPropagation();
        self.toggleFilter();
      });
      var close = document.getElementById('filter-close');
      if (close) close.addEventListener('click', function () { self.closeFilter(); });
      panel.addEventListener('change', function (e) {
        // KF-175: handle both select dropdowns (by id) and radio buttons (by name)
        if (e.target.id === 'f-user' || e.target.id === 'f-color' || e.target.id === 'f-date' ||
            e.target.name === 'f-user' || e.target.name === 'f-color' || e.target.name === 'f-date' || e.target.name === 'f-label') {
          self.applyFilter();
          self.maybeSaveFilter();
        } else if (e.target.id === 'filter-remember') {
          self.maybeSaveFilter();
        }
      });
      this.restoreFilter();
      // KF-215: dock the panel below the top bars. Measure the dark topbar
      // + gray board-header row so the panel's own header (Filter title +
      // close button) renders below them instead of sliding underneath —
      // the panel can then never cover its own toggle, and the close
      // button stays reachable. Recompute on resize for font/zoom changes.
      this.positionFilterPanel();
      window.addEventListener('resize', function () { self.positionFilterPanel(); });
    },

    positionFilterPanel: function () {
      var panel = document.getElementById('filter-panel');
      var topbar = document.querySelector('.topbar');
      var headerRow = document.querySelector('.board-header-row');
      if (!panel || !topbar || !headerRow) return;
      var offset = topbar.offsetHeight + headerRow.offsetHeight;
      panel.style.top = offset + 'px';
      panel.style.height = 'calc(100vh - ' + offset + 'px)';
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
        this.filterValue('f-date') !== 'all' ||
        this.filterLabels().length > 0;
    },
    filterValue: function (name) {
      // KF-175: support both select dropdowns (new) and radio buttons (legacy)
      var select = document.getElementById(name);
      if (select && select.tagName === 'SELECT') return select.value;
      var el = document.querySelector('input[name="' + name + '"]:checked');
      return el ? el.value : 'all';
    },
    setRadio: function (name, value) {
      if (!value) return;
      // KF-175: support select dropdowns
      var select = document.getElementById(name);
      if (select && select.tagName === 'SELECT') { select.value = value; return; }
      var el = document.querySelector('input[name="' + name + '"][value="' + value + '"]');
      if (el) el.checked = true;
    },

    applyFilter: function () {
      var user = this.filterValue('f-user');
      var color = this.filterValue('f-color');
      var date = this.filterValue('f-date');
      var labels = this.filterLabels();
      var self = this;
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
        // KF-159: date filters evaluate the card's data-due-at (RFC3339).
        if (!self.dateMatches(date, card.dataset.dueAt)) show = false;
        // KF-165: label filters match the card's data-labels JSON array.
        // Checking several labels narrows to cards carrying every one
        // (KanbanFlow parity).
        if (show && labels.length) {
          var cardLabels = [];
          try { cardLabels = JSON.parse(card.dataset.labels || '[]'); } catch (e) { cardLabels = []; }
          var allPresent = labels.every(function (l) { return cardLabels.indexOf(l) !== -1; });
          if (!allPresent) show = false;
        }
        card.style.display = show ? '' : 'none';
      });
      this.refreshFilteredCounts();
      this.syncFilterBtn();
    },
    // KF-161: column header counts reflect the currently filtered (visible)
    // card set, KanbanFlow parity. When no filter is active the
    // server-rendered totals are restored from th.dataset.taskCount.
    refreshFilteredCounts: function () {
      var filtering = this.isFiltering();
      document.querySelectorAll('.columnHeader[data-column-id]').forEach(function (th) {
        var colId = th.dataset.columnId;
        var count;
        if (filtering) {
          count = 0;
          var sel = '.task-list[data-column-id="' + cssEscape(colId) + '"] .task-card';
          document.querySelectorAll(sel).forEach(function (card) {
            if (card.style.display !== 'none') count++;
          });
        } else {
          count = parseInt(th.dataset.taskCount, 10) || 0;
        }
        var wip = th.dataset.wipLimit ? parseInt(th.dataset.wipLimit, 10) : null;
        var el = th.querySelector('.columnHeader-count');
        if (el) el.textContent = wip ? count + ' / ' + wip : String(count);
        // KF-297: also update the badge element with filtered counts
        var badge = th.querySelector('.columnHeader-countBadge');
        if (badge) badge.textContent = String(count);
        // Mirror updateColumnCount's WIP-violation warning for the visible set.
        var warn = wip != null && count > wip;
        th.classList.toggle('columnHeader--warning', warn);
        th.classList.toggle('columnHeader--limitWarning', warn);
        syncWarningLine(th, warn);
      });
    },
    // KF-159: due-date matching for the filter panel. Undated tasks are
    // hidden for every specific range; only "Show all" keeps them and
    // "No due date" shows only them (KanbanFlow parity).
    dateMatches: function (value, dueAt) {
      if (value === 'all') return true;
      if (!dueAt) return value === 'none';
      if (value === 'none') return false;
      var d = new Date(dueAt);
      if (isNaN(d.getTime())) return false;
      var day = new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
      var now = new Date();
      var today = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
      var DAY = 86400000;
      // KF-219: "Overdue" compares the full due timestamp, not the date —
      // a task due later today is not overdue (KanbanFlow parity).
      if (value === 'overdue') return d.getTime() < Date.now();
      if (value === 'today') return day === today;
      if (value === 'tomorrow') return day === today + DAY;
      if (value.indexOf('month:') === 0) {
        var mp = value.slice(6).split('-');
        return d.getFullYear() === parseInt(mp[0], 10) &&
          d.getMonth() === parseInt(mp[1], 10);
      }
      if (value.indexOf('year:') === 0) {
        return d.getFullYear() === parseInt(value.slice(5), 10);
      }
      var n = parseInt(value, 10);
      if (!isNaN(n)) {
        // "Due in N days": due within [today, today + N days].
        return day >= today && day <= today + n * DAY;
      }
      return true;
    },

    // KF-165: populate the Labels section of the filter panel from the
    // board's label set (GET /api/boards/:id/labels). Each label gets a
    // checkbox; checking labels narrows the board (see applyFilter).
    loadFilterLabels: function () {
      var self = this;
      var wrap = document.getElementById('filter-labels-list');
      if (!wrap || wrap.dataset.loaded) return;
      wrap.dataset.loaded = '1';
      api('/api/boards/' + encodeURIComponent(boardId()) + '/labels', 'GET')
        .then(function (res) { return res.ok ? res.json() : []; })
        .then(function (labels) {
        wrap.innerHTML = '';
        if (!labels || !labels.length) {
          wrap.innerHTML = '<span class="empty-note">No labels on this board yet.</span>';
          return;
        }
        labels.forEach(function (l) {
          var label = document.createElement('label');
          label.className = 'filter-opt';
          var input = document.createElement('input');
          input.type = 'checkbox';
          input.name = 'f-label';
          input.value = l;
          label.appendChild(input);
          label.appendChild(document.createTextNode(' ' + l));
          wrap.appendChild(label);
        });
        // A remembered label filter applies once its options exist.
        self.restoreLabelFilter();
      }).catch(function () {
        wrap.innerHTML = '<span class="empty-note">Could not load labels.</span>';
      });
    },
    // KF-165: labels currently checked in the filter panel.
    filterLabels: function () {
      var sel = [];
      document.querySelectorAll('input[name="f-label"]:checked').forEach(function (el) { sel.push(el.value); });
      return sel;
    },
    // KF-165: toggle the filter for one label — called when a card's
    // label chip is clicked (KanbanFlow parity: clicking a label filters
    // the board instead of opening the task).
    toggleLabelFilter: function (label) {
      var self = this;
      var wrap = document.getElementById('filter-labels-list');
      var found = false;
      if (wrap) {
        var boxes = wrap.querySelectorAll('input[name="f-label"]');
        for (var i = 0; i < boxes.length; i++) {
          if (boxes[i].value === label) {
            boxes[i].checked = !boxes[i].checked;
            found = true;
            break;
          }
        }
      }
      if (!found) {
        // Labels haven't loaded yet (or label is new): remember it and
        // apply once loadFilterLabels populates the checkboxes.
        this._pendingLabelFilter = [label];
      }
      this.openFilter();
      this.applyFilter();
      this.maybeSaveFilter();
    },
    // KF-165: check the remembered labels once the label options load.
    restoreLabelFilter: function () {
      var pending = this._pendingLabelFilter;
      this._pendingLabelFilter = null;
      if (!pending || !pending.length) return;
      var wrap = document.getElementById('filter-labels-list');
      if (!wrap) return;
      var boxes = wrap.querySelectorAll('input[name="f-label"]');
      for (var i = 0; i < boxes.length; i++) {
        if (pending.indexOf(boxes[i].value) !== -1) boxes[i].checked = true;
      }
      this.applyFilter();
    },

    maybeSaveFilter: function () {
      var remember = document.getElementById('filter-remember');
      try {
        if (remember && remember.checked) {
          window.localStorage.setItem('chipflow-filter', JSON.stringify({
            user: this.filterValue('f-user'),
            color: this.filterValue('f-color'),
            date: this.filterValue('f-date'),
            labels: this.filterLabels()
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
        // Label checkboxes load async — apply them when they arrive.
        this._pendingLabelFilter = (f.labels && f.labels.length) ? f.labels : null;
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
      var layoutClose = document.getElementById('layout-view-close');
      if (layoutClose) layoutClose.addEventListener('click', function () { self.exitLayout(); });
      var layoutBack = document.getElementById('layout-back');
      if (layoutBack) layoutBack.addEventListener('click', function () { self.exitLayout(); });
      var layoutAddCol = document.getElementById('layout-add-column');
      if (layoutAddCol) layoutAddCol.addEventListener('click', function () {
        // KF-122: toolbar button removed; call the dialog directly.
        openAddColumnDialog('end');
      });
      // KF-246: green "Add column" button at the right end of the column list;
      // same add-column flow as the header button.
      var layoutAddColEnd = document.getElementById('layout-add-column-end');
      if (layoutAddColEnd) layoutAddColEnd.addEventListener('click', function () {
        openAddColumnDialog('end');
      });
      var layoutAddSwim = document.getElementById('layout-add-swimlane');
      if (layoutAddSwim) layoutAddSwim.addEventListener('click', function () {
        // KF-122: toolbar button removed; call directly.
        addSwimlane();
      });
    },

    // KF-032: layout-edit view ("Layout: <board>") with Add column / Add
    // swimlane actions. The timer pill is hidden while this view is open
    // (KanbanFlow parity, frame-review section 9).
    editLayout: function () {
      var board = document.querySelector('main.board-wrap');
      var view = document.getElementById('layout-view');
      if (!board || !view) return;
      board.hidden = true;
      view.hidden = false;
      var pill = document.getElementById('timer-pill');
      if (pill) pill.hidden = true;
      // KF-180: wire drag-reorder for the arrange lists.
      initLayoutSortables();
    },

    exitLayout: function () {
      var board = document.querySelector('main.board-wrap');
      var view = document.getElementById('layout-view');
      if (board) board.hidden = false;
      if (view) view.hidden = true;
      // renderPill restores the pill's own visibility state.
      if (typeof TimerUI !== 'undefined' && TimerUI.renderPill) TimerUI.renderPill();
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
      // KF-353: keep the menu's toggle switch in sync.
      var item = document.querySelector('#board-menu [data-bm="dark"]');
      if (item) item.setAttribute('aria-checked', on ? 'true' : 'false');
      toast(on ? 'Dark mode on.' : 'Dark mode off.');
    },
    toggleLargeNames: function () {
      var on = !document.body.classList.contains('large-names');
      document.body.classList.toggle('large-names', on);
      this.setPref('chipflow-large-names', on);
      // KF-353: keep the menu's toggle switch in sync.
      var item = document.querySelector('#board-menu [data-bm="large-names"]');
      if (item) item.setAttribute('aria-checked', on ? 'true' : 'false');
      toast(on ? 'Large task names on.' : 'Large task names off.');
    },
    // KF-222: the Color legend is a per-board Menu toggle (KanbanFlow shows
    // no legend by default). The server renders the footer only when
    // legend_visible is ON — no footer markup at all when OFF — so persist
    // the new state, then reload to apply it.
    toggleLegend: function () {
      var item = document.querySelector('#board-menu [data-bm="legend"]');
      var on = item ? item.getAttribute('aria-checked') !== 'true' : true;
      var id = boardId();
      if (!id) return;
      if (item) item.setAttribute('aria-checked', on ? 'true' : 'false');
      api('/api/boards/' + encodeURIComponent(id) + '/config', 'PUT', { legend_visible: on })
        .then(function (res) { return res.json().catch(function () { return null; }).then(function (body) { return { res: res, body: body }; }); })
        .then(function (pair) {
          var ok = pair.res.ok && pair.body && pair.body.legend_visible === on;
          if (!ok) {
            // Roll back the optimistic toggle so the UI matches the server.
            if (item) item.setAttribute('aria-checked', on ? 'false' : 'true');
            toast('Could not save the legend setting.');
            return;
          }
          toast(on ? 'Color legend on.' : 'Color legend off.');
          window.location.reload();
        })
        .catch(function () {
          if (item) item.setAttribute('aria-checked', on ? 'false' : 'true');
          toast('Could not save the legend setting.');
        });
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

  // ---------- Boards sidebar (KF-088) ----------
  //
  // Persistent left sidebar on board pages: "Boards" header, search box,
  // "Drag to add to Favorites" hint, Favorites section + All boards.
  // Favorites persist in settings.favorite_boards via PUT /api/settings.

  function bsFavorites() {
    var out = [];
    document.querySelectorAll('#bs-favorites .bs-item').forEach(function (li) {
      out.push(li.getAttribute('data-board-id'));
    });
    return out;
  }

  function saveBsFavorites(next) {
    fetch('/api/settings', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      credentials: 'same-origin',
      body: JSON.stringify({ favorite_boards: next }),
    }).then(function (r) {
      if (!r.ok) toast('Could not save favorites.');
    }).catch(function () { toast('Could not save favorites.'); });
  }

  function renderBsFavorites(ids) {
    // Rebuild the favorites list from the All-boards rows so names stay in
    // sync with the server render.
    var fav = document.getElementById('bs-favorites');
    if (!fav) return;
    var byId = {};
    document.querySelectorAll('#bs-all .bs-item').forEach(function (li) {
      byId[li.getAttribute('data-board-id')] = li.getAttribute('data-board-name');
    });
    var html = '';
    ids.forEach(function (id) {
      var name = byId[id];
      if (name == null) return;
      html += '<li class="bs-item" data-board-id="' + escapeHtml(id) +
        '" data-board-name="' + escapeHtml(name) + '">' +
        '<a href="/b/' + encodeURIComponent(id) + '">' + escapeHtml(name) + '</a>' +
        '<button type="button" class="bs-unfav" data-unfav="' + escapeHtml(id) +
        '" title="Remove from favorites">&times;</button></li>';
    });
    fav.innerHTML = html || '<li class="bs-empty">No favorites yet.</li>';
    wireBsUnfav();
    updateBsFavSection();
  }

  function wireBsUnfav() {
    document.querySelectorAll('#bs-favorites [data-unfav]').forEach(function (btn) {
      btn.onclick = function () {
        var id = btn.getAttribute('data-unfav');
        var next = bsFavorites().filter(function (x) { return x !== id; });
        renderBsFavorites(next);
        saveBsFavorites(next);
      };
    });
  }

  function updateBsFavSection() {
    var section = document.getElementById('bs-fav-section');
    if (!section) return;
    var empty = !document.querySelector('#bs-favorites .bs-item');
    section.classList.toggle('bs-fav-empty', empty);
  }

  // KF-195: account menu (Sign out via POST /logout).
  function initAccountMenu() {
    var btn = document.getElementById('account-btn');
    var menu = document.getElementById('account-menu');
    if (!btn || !menu) return;
    function placeMenu() {
      var r = btn.getBoundingClientRect();
      menu.style.top = (r.bottom + 6) + 'px';
      menu.style.right = Math.max(8, window.innerWidth - r.right) + 'px';
      menu.style.left = 'auto';
    }
    function closeMenu() {
      menu.hidden = true;
      btn.setAttribute('aria-expanded', 'false');
    }
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
    document.addEventListener('click', function (e) {
      if (!menu.hidden &&
          !e.target.closest('#account-menu') &&
          !e.target.closest('#account-btn')) {
        closeMenu();
      }
    });
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape' && !menu.hidden) closeMenu();
    });
  }

  // KF-310: notifications panel popover (empty state; no notification model
  // exists). Mirrors the account-menu open/dismiss pattern.
  function initNotificationsPanel() {
    var btn = document.getElementById('notifications-btn');
    var panel = document.getElementById('notifications-panel');
    if (!btn || !panel) return;
    function placePanel() {
      var r = btn.getBoundingClientRect();
      panel.style.top = (r.bottom + 6) + 'px';
      panel.style.right = Math.max(8, window.innerWidth - r.right) + 'px';
      panel.style.left = 'auto';
    }
    function closePanel() {
      panel.hidden = true;
      btn.setAttribute('aria-expanded', 'false');
    }
    function closeAccountMenu() {
      // KF-310: one topbar popover at a time. The account button's own
      // handler stopPropagation()s, so its document-level closer never runs
      // for the bell click.
      var am = document.getElementById('account-menu');
      var ab = document.getElementById('account-btn');
      if (am && !am.hidden) {
        am.hidden = true;
        if (ab) ab.setAttribute('aria-expanded', 'false');
      }
    }
    btn.addEventListener('click', function (e) {
      e.stopPropagation();
      closeAccountMenu();
      if (panel.hidden) {
        placePanel();
        panel.hidden = false;
        btn.setAttribute('aria-expanded', 'true');
      } else {
        closePanel();
      }
    });
    // Capture phase: the account button and the timer pill stopPropagation(),
    // so a bubble-phase closer would miss those outside clicks.
    document.addEventListener('click', function (e) {
      if (!panel.hidden &&
          !e.target.closest('#notifications-panel') &&
          !e.target.closest('#notifications-btn')) {
        closePanel();
      }
    }, true);
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape' && !panel.hidden) closePanel();
    });
  }

  // KF-236: top-bar reports menu (KanbanFlow parity): the 15 report
  // destinations. Items carry the same data-report keys as the board
  // Menu → Reports submenu (KF-136) and route through the same
  // BoardChrome.reportAction, so a click does exactly what the submenu
  // item does today. Mirrors the notifications-panel open/dismiss pattern.
  function initTopbarReportsMenu() {
    var btn = document.getElementById('topbar-reports-btn');
    var menu = document.getElementById('topbar-reports-menu');
    if (!btn || !menu) return;
    function placeMenu() {
      var r = btn.getBoundingClientRect();
      menu.style.top = (r.bottom + 6) + 'px';
      menu.style.right = Math.max(8, window.innerWidth - r.right) + 'px';
      menu.style.left = 'auto';
    }
    function closeMenu() {
      menu.hidden = true;
      btn.setAttribute('aria-expanded', 'false');
    }
    function closeOtherPopovers() {
      // One topbar popover at a time.
      var pairs = [['notifications-panel', 'notifications-btn'],
                   ['account-menu', 'account-btn']];
      pairs.forEach(function (pair) {
        var m = document.getElementById(pair[0]);
        var b = document.getElementById(pair[1]);
        if (m && !m.hidden) {
          m.hidden = true;
          if (b) b.setAttribute('aria-expanded', 'false');
        }
      });
    }
    btn.addEventListener('click', function (e) {
      e.stopPropagation();
      closeOtherPopovers();
      if (menu.hidden) {
        placeMenu();
        menu.hidden = false;
        btn.setAttribute('aria-expanded', 'true');
      } else {
        closeMenu();
      }
    });
    menu.addEventListener('click', function (e) {
      var item = e.target.closest('[data-report]');
      if (!item) return;
      e.stopPropagation();
      var key = item.getAttribute('data-report');
      var label = item.textContent.trim();
      closeMenu();
      BoardChrome.reportAction(key, label);
    });
    // Capture phase: sibling popover triggers stopPropagation(), so a
    // bubble-phase closer would miss those outside clicks.
    document.addEventListener('click', function (e) {
      if (!menu.hidden &&
          !e.target.closest('#topbar-reports-menu') &&
          !e.target.closest('#topbar-reports-btn')) {
        closeMenu();
      }
    }, true);
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape' && !menu.hidden) closeMenu();
    });
  }

  function initBoardsDropdown() {
    var dropdown = document.getElementById('boards-dropdown');
    if (!dropdown) return;
    // KF-330: Boards dropdown toggle (light menu, KanbanFlow parity)
    var dropdownBtn = document.getElementById('boards-dropdown-btn');
    
    function openDropdown() {
      dropdown.hidden = false;
      // Set aria-expanded attribute for accessibility
      if (dropdownBtn) {
        dropdownBtn.setAttribute('aria-expanded', 'true');
      }
    }
    
    function closeDropdown() {
      dropdown.hidden = true;
      // Set aria-expanded attribute for accessibility
      if (dropdownBtn) {
        dropdownBtn.setAttribute('aria-expanded', 'false');
      }
    }
    
    function toggleDropdown() {
      if (dropdown.hidden) {
        openDropdown();
      } else {
        closeDropdown();
      }
    }
    
    // Toggle dropdown on button click
    if (dropdownBtn) {
      dropdownBtn.addEventListener('click', toggleDropdown);
      // Set initial aria-expanded attribute
      dropdownBtn.setAttribute('aria-expanded', 'false');
    }
    
    // Close dropdown when clicking outside
    function closeOnOutsideClick(e) {
      if (!dropdown.contains(e.target) && !dropdownBtn.contains(e.target)) {
        closeDropdown();
      }
    }
    
    document.addEventListener('click', closeOnOutsideClick);
    
    // Close dropdown on Escape key
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape') {
        closeDropdown();
      }
    });
    
    // Keep the existing board search functionality
    var search = document.getElementById('bs-search');
    if (search) {
      search.addEventListener('input', function () {
        var q = search.value.trim().toLowerCase();
        document.querySelectorAll('#boards-dropdown .bs-item').forEach(function (li) {
          var name = (li.getAttribute('data-board-name') || '').toLowerCase();
          li.hidden = q !== '' && name.indexOf(q) === -1;
        });
      });
    }
    
    // Keep the existing drag and drop functionality for favorites
    wireBsUnfav();
    updateBsFavSection();
    
    // Drag to add to Favorites.
    var dropzone = document.getElementById('bs-favorites');
    if (dropzone) {
      document.querySelectorAll('#bs-all .bs-item').forEach(function (li) {
        li.addEventListener('dragstart', function (e) {
          e.dataTransfer.setData('text/board-id', li.getAttribute('data-board-id'));
          e.dataTransfer.effectAllowed = 'copy';
        });
      });
      dropzone.addEventListener('dragover', function (e) {
        e.preventDefault();
        e.dataTransfer.dropEffect = 'copy';
        dropzone.classList.add('bs-drop-hover');
      });
      dropzone.addEventListener('dragleave', function () {
        dropzone.classList.remove('bs-drop-hover');
      });
      dropzone.addEventListener('drop', function (e) {
        e.preventDefault();
        dropzone.classList.remove('bs-drop-hover');
        var id = e.dataTransfer.getData('text/board-id');
        if (!id) return;
        var next = bsFavorites();
        if (next.indexOf(id) === -1) {
          next.push(id);
          renderBsFavorites(next);
          saveBsFavorites(next);
        }
      });
    }
  }

  // KF-224: card icon-row section expand/collapse. Each icon toggles its
  // card section; the state persists per task in localStorage. Defaults
  // match KanbanFlow's board: description collapsed, due dates and
  // subtasks expanded. IIFE-level so quick-add's insert path can reuse it.
  var CARD_SEC_DEFAULTS = { description: false, duedates: true, subtasks: true };
  function cardSecState(taskId) {
    var st = { description: CARD_SEC_DEFAULTS.description, duedates: CARD_SEC_DEFAULTS.duedates, subtasks: CARD_SEC_DEFAULTS.subtasks };
    try {
      var raw = localStorage.getItem('chipflow.cardsec.' + taskId);
      if (raw) {
        var parsed = JSON.parse(raw);
        Object.keys(st).forEach(function (k) {
          if (typeof parsed[k] === 'boolean') st[k] = parsed[k];
        });
      }
    } catch (e) { /* corrupted entry: fall back to defaults */ }
    return st;
  }
  function applyCardSecState(card) {
    var taskId = card.getAttribute('data-task-id');
    if (!taskId) return;
    var st = cardSecState(taskId);
    Object.keys(st).forEach(function (sec) {
      var body = card.querySelector('[data-sec-body="' + sec + '"]');
      if (body) body.hidden = !st[sec];
    });
  }
  function refreshCardSecStates() {
    document.querySelectorAll('.task-card').forEach(applyCardSecState);
  }

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
    initBoardsDropdown();
    initAccountMenu(); // KF-195
    initNotificationsPanel(); // KF-310
    initTopbarReportsMenu(); // KF-236
    refreshCardSecStates(); // KF-224: apply persisted card section state
    // Cards inserted later via htmx swaps need the same treatment.
    document.addEventListener('htmx:afterSwap', function () { refreshCardSecStates(); });

    // Icon clicks: buttons/inputs never reach the card-click opener below,
    // so no stopPropagation dance is needed.
    document.addEventListener('click', function (e) {
      var ico = e.target.closest('.card-ico[data-sec]');
      if (ico) {
        var card = ico.closest('.task-card');
        var sec = ico.getAttribute('data-sec');
        if (!card || !sec) return;
        var taskId = card.getAttribute('data-task-id');
        if (sec === 'comments') { // "Click to view comments."
          if (taskId) openModal(taskId);
          return;
        }
        var body = card.querySelector('[data-sec-body="' + sec + '"]');
        if (!body || !taskId) return;
        var st = cardSecState(taskId);
        st[sec] = body.hidden; // hidden now => shown after toggle
        body.hidden = !st[sec];
        try { localStorage.setItem('chipflow.cardsec.' + taskId, JSON.stringify(st)); } catch (e2) {}
        return;
      }
      // "N hidden (M done)" — expand/collapse the full inline subtask list.
      var more = e.target.closest('.card-sub-more');
      if (more) {
        var subsec = more.closest('.card-subsec');
        if (subsec) subsec.classList.toggle('card-sub-expanded');
      }
    });

    // Inline subtask checkboxes: toggle via the API, then refresh the
    // "N hidden (M done)" summary from the card's data attributes.
    document.addEventListener('change', function (e) {
      var check = e.target.closest('.card-sub-check');
      if (!check) return;
      var card = check.closest('.task-card');
      if (!card) return;
      var taskId = card.getAttribute('data-task-id');
      var subId = check.getAttribute('data-subtask-id');
      if (!taskId || !subId) return;
      api('/api/tasks/' + encodeURIComponent(taskId) + '/subtasks/' +
          encodeURIComponent(subId), 'PATCH', { done: check.checked })
        .then(function (res) {
          if (!res.ok) throw new Error('toggle failed');
          var total = parseInt(card.getAttribute('data-subtasks-total') || '0', 10) || 0;
          var done = parseInt(card.getAttribute('data-subtasks-done') || '0', 10) || 0;
          done = Math.max(0, Math.min(total, done + (check.checked ? 1 : -1)));
          card.setAttribute('data-subtasks-done', String(done));
          var visibleDone = card.querySelectorAll('.card-sub-check:checked').length;
          var hiddenTotal = Math.max(0, total - 2);
          var hiddenDone = Math.max(0, done - visibleDone);
          var moreEl = card.querySelector('.card-sub-more');
          if (moreEl && hiddenTotal > 0) {
            moreEl.textContent = hiddenTotal + ' hidden (' + hiddenDone + ' done)';
          }
        })
        .catch(function () {
          check.checked = !check.checked;
          toast('Could not update subtask.');
        });
    });

    // Click a card to open its modal. Drags, and clicks on interactive
    // elements inside a card, are ignored.
    document.addEventListener('click', function (e) {
      if (dragging) return;
      if (e.target.closest('button, a, input, select, textarea, form, .task-modal, .timer-popup, .why-stop-menu, .menu-pop, .tm-menu, .dlg-overlay, .card-sub, .card-sub-more')) return;
      var card = e.target.closest('.task-card');
      if (!card) return;
      // KF-165: clicking a label chip filters the board by that label
      // (KanbanFlow parity) instead of opening the task modal.
      var chip = e.target.closest('.card-label');
      if (chip && chip.textContent) {
        BoardChrome.toggleLabelFilter(chip.textContent.trim());
        return;
      }
      if (card.dataset.taskId) openModal(card.dataset.taskId);
    });

    // Enter on a focused card opens it (cards are tabindex=0).
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Enter' && e.target.classList &&
          e.target.classList.contains('task-card')) {
        openModal(e.target.dataset.taskId);
      }
    });

    // KF-117: Task-URL deep links — copyTaskUrl produces
    // origin + '/b/' + boardId + '#task-' + id. On board load (and on
    // hashchange) scroll to the card and open its modal.
    function openTaskFromHash() {
      var m = /^#task-([\w-]+)$/.exec(window.location.hash || '');
      if (!m) return;
      var id = m[1];
      var card = document.querySelector('.task-card[data-task-id="' + cssEscape(id) + '"]');
      if (card) {
        card.scrollIntoView({ behavior: 'smooth', block: 'center' });
        card.focus({ preventScroll: true });
      }
      openModal(id);
    }
    window.addEventListener('hashchange', openTaskFromHash);
    openTaskFromHash();
  }

  document.addEventListener('DOMContentLoaded', function () {
    if (document.querySelector('.board-wrap')) initBoard();
    initEntryEdit();
    initTaskExtras();
    TimerLogPage.init();
    TimerStatsPage.init();
    TimerUI.init();
    BoardChrome.init();
    initBoardDeletePage();
  });
  // In case app.js runs after DOMContentLoaded (defer ordering):
  if (document.readyState !== 'loading') {
    if (document.querySelector('.board-wrap')) initBoard();
    initEntryEdit();
    initTaskExtras();
    TimerLogPage.init();
    TimerStatsPage.init();
    TimerUI.init();
    BoardChrome.init();
    initBoardDeletePage();
  }

  // Exposed for inline handlers and debugging.
  window.closeModal = closeModal;
  window.TimerUI = TimerUI;
  // KF-158: the timer popup's footer Settings button calls
  // TimerSettings.open() from an inline onclick handler, so it must be
  // reachable as a global.
  window.TimerSettings = TimerSettings;
  window.ManualTime = ManualTime;
  window.EditEntry = EditEntry;
  window.TimerLogPage = TimerLogPage;
  window.TimerStatsPage = TimerStatsPage;
  window.ExportPage = ExportPage;
  window.TaskCountPage = TaskCountPage;
  window.ThroughputPage = ThroughputPage;
  window.BurndownPage = BurndownPage;
  window.CumulativeFlowPage = CumulativeFlowPage;
  window.CycleLeadPage = CycleLeadPage;
  window.DueDatePage = DueDatePage;
  window.BoardHistoryPage = BoardHistoryPage;
  window.CalendarPage = CalendarPage;
  window.MonteCarloPage = MonteCarloPage;
  window.TimeEstimatePage = TimeEstimatePage;
  window.BoardChrome = BoardChrome;

  // KF-278: hovering Time spent shows a tooltip and the "Add time entry /
  // Open time log" menu. Both nodes are created once and reused by every
  // modal open.
  var timeSpentHover = null;

  function timeSpentHoverNodes() {
    if (timeSpentHover) return timeSpentHover;
    var tooltip = document.createElement('div');
    tooltip.className = 'tm-time-tooltip';
    tooltip.textContent = 'You are selecting this task';
    tooltip.hidden = true;
    var menu = document.createElement('div');
    menu.className = 'tm-time-menu';
    menu.hidden = true;
    menu.innerHTML =
      '<button type="button" data-time-act="add">Add time entry</button>' +
      '<button type="button" data-time-act="log">Open time log</button>';
    document.body.appendChild(tooltip);
    document.body.appendChild(menu);
    var hideTimer = null;
    var nodes = {
      show: function (cell) {
        window.clearTimeout(hideTimer);
        var r = cell.getBoundingClientRect();
        tooltip.hidden = false;
        tooltip.style.left = (r.left + r.width / 2 - tooltip.offsetWidth / 2) + 'px';
        tooltip.style.top = (r.top - tooltip.offsetHeight - 4) + 'px';
        menu.hidden = false;
        menu.style.left = r.left + 'px';
        menu.style.top = (r.bottom + 2) + 'px';
      },
      hide: function () {
        window.clearTimeout(hideTimer);
        tooltip.hidden = true;
        menu.hidden = true;
      },
      hideSoon: function () {
        window.clearTimeout(hideTimer);
        tooltip.hidden = true;
        hideTimer = window.setTimeout(nodes.hide, 200);
      },
    };
    menu.addEventListener('mouseenter', function () { window.clearTimeout(hideTimer); });
    menu.addEventListener('mouseleave', nodes.hideSoon);
    menu.addEventListener('click', function (e) {
      var btn = e.target.closest('[data-time-act]');
      if (!btn) return;
      e.stopPropagation();
      nodes.hide();
      var id = modalTaskId();
      if (!id) return;
      if (btn.getAttribute('data-time-act') === 'add') {
        var nameInput = document.getElementById('modal-name');
        ManualTime.open(id, nameInput ? nameInput.value : null);
      } else {
        openModalSubview('time-log');
      }
    });
    timeSpentHover = nodes;
    return nodes;
  }

  function wireTimeSpentHover() {
    var cell = document.querySelector('.tm-cell[data-cell="time-spent"]');
    if (!cell) return;
    var nodes = timeSpentHoverNodes();
    nodes.hide();
    cell.addEventListener('mouseenter', function () { nodes.show(cell); });
    cell.addEventListener('mouseleave', nodes.hideSoon);
  }
})();
