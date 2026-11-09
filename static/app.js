/* Pomb frontend glue: KanbanFlow-style board (drag-and-drop, column and
 * swimlane menus, task modal), plus the global timer.
 * No framework — plain JS plus SortableJS (drag-and-drop) and htmx.
 *
 * The timer is server-side: one active session, so it survives page
 * reloads. This file polls it, renders the header pill + popup, and
 * handles the "why did you stop?" flow. */
(function () {
  'use strict';

  if (window.htmx) {
    window.htmx.config.allowEval = false;
    window.htmx.config.includeIndicatorStyles = false;
  }

  var dragGroup = null;
  var accountButton = document.getElementById('account-btn');
  var accountPrefs = {
    hour12: !accountButton || accountButton.dataset.timeFormat !== '24h',
    weekStart: accountButton && accountButton.dataset.weekStart === 'monday' ? 1 : 0,
  };

  function daysIntoWeek(d) {
    return (d.getDay() - accountPrefs.weekStart + 7) % 7;
  }

  function weekdaysFromWeekStart(names) {
    return names.slice(accountPrefs.weekStart).concat(names.slice(0, accountPrefs.weekStart));
  }

  function clockText(d) {
    var h = d.getHours();
    var m = d.getMinutes();
    var minutes = (m < 10 ? '0' : '') + m;
    if (!accountPrefs.hour12) return (h < 10 ? '0' : '') + h + ':' + minutes;
    return (h % 12 || 12) + ':' + minutes + ' ' + (h < 12 ? 'AM' : 'PM');
  }

  LocalTime.use(accountPrefs);
  LocalTime.watch();

  var dragging = false;   // true while a Sortable drag is in flight (suppresses card clicks)
  var modalDirty = false; // set when the modal changed something; reloads the board on close
  // KF-334: the Move task dialog's task and target, held between
  // openMoveDialog() and doMoveTask().
  var moveTargetColumnId = null;
  var moveSwimlaneId = null;
  var moveTaskId = null;
  var moveBulkIds = null;

  // ---------- small helpers ----------

  function boardId() {
    var main = document.querySelector('.board-wrap');
    return main ? main.dataset.boardId : null;
  }

  function modalBoardId() {
    var shell = document.querySelector('.board-settings-page[data-board-id]');
    return boardId() || (shell && shell.querySelector('.bshell-report') ? shell.dataset.boardId : null);
  }

  function modalTaskBoard() {
    var modal = document.querySelector('#modal-root .task-modal');
    var id = modal && modal.dataset ? modal.dataset.boardId : null;
    return id ? { id: id, name: modal.dataset.boardName || id } : null;
  }

  function boardNameFor(id) {
    var own = modalTaskBoard();
    if (own && own.id === id) return own.name;
    var title = document.querySelector('.board-bar-title');
    var text = title ? title.textContent.trim() : '';
    return text || document.title.replace(/ — Pomb$/, '');
  }

  function api(path, method, data) {
    return fetch(path, {
      method: method,
      headers: { 'Content-Type': 'application/json' },
      body: data === undefined ? undefined : JSON.stringify(data),
    });
  }

  // KF-517: read the response body before the page reloads, so the reload
  // does not abort the request ("net::ERR_ABORTED" in the browser).
  function drained(res) {
    return res.text().then(function () { return res; }, function () { return res; });
  }

  function escapeHtml(s) {
    return String(s)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  var toastTimer = null;
  function toast(message, detail) {
    var el = document.getElementById('toast');
    if (!el) return;
    el.textContent = message;
    if (detail) {
      var sub = document.createElement('span');
      sub.className = 'toast-sub';
      sub.textContent = detail;
      el.appendChild(sub);
    }
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
        disabled: !!document.querySelector('.board-wrap[data-role="viewer"]'),
        animation: 150,
        draggable: '.task-card',
        ghostClass: 'sortable-placeholder',
        setData: function (dataTransfer, card) {
          dataTransfer.setData('Text', card.textContent);
          dataTransfer.setDragImage(dragHelper(card), 12, 12);
        },
        onStart: function (evt) { dragging = true; dragGroup = dragGroupIds(evt.item); },
        onMove: function (evt) { markDoneTarget(evt.to); },
        onEnd: function (evt) {
          window.setTimeout(function () { dragging = false; }, 80);
          markDoneTarget(null);
          if (window.BoardChrome && BoardChrome.unfold) {
            BoardChrome.unfold();
            window.setTimeout(function () { BoardChrome.refold(); }, 0);
          }
          var card = evt.item;
          var toList = evt.to;
          var fromList = evt.from;
          // KF-441: a collapsed strip takes no cards, so a drop on it puts
          // the card back where it was and moves nothing.
          if (droppedOnCollapsedStrip()) {
            var cardsHere = Array.prototype.filter.call(fromList.querySelectorAll('.task-card'), function (el) { return el !== card; });
            if (cardsHere[evt.oldDraggableIndex]) fromList.insertBefore(card, cardsHere[evt.oldDraggableIndex]);
            else if (cardsHere.length) cardsHere[cardsHere.length - 1].after(card);
            return;
          }
          // KF-371: a sorted column's order is its sort order, so a drop
          // within it snaps straight back.
          if (toList === fromList && toList.dataset.sorted === '1') {
            if (evt.oldDraggableIndex !== evt.newDraggableIndex) {
              var rest = Array.prototype.filter.call(fromList.querySelectorAll('.task-card'), function (el) { return el !== card; });
              fromList.insertBefore(card, rest[evt.oldDraggableIndex] || null);
              toast('This column is sorted, so its order follows the sorting.');
            }
            return;
          }
          var cards = Array.prototype.slice.call(toList.querySelectorAll('.task-card'));
          var position = cards.indexOf(card);
          var group = dragGroup;
          dragGroup = null;
          if (group) {
            var others = cards.filter(function (el) { return el === card || !isSelected(el); });
            clearSelection();
            moveSelectionTo(group, toList, others.indexOf(card));
            return;
          }
          var estimate = cardEstimateMinutes(card);
          var fromCol = fromList.dataset.columnId;
          var toCol = toList.dataset.columnId;
          // Done-column date grouping can't be fixed by swapping one card;
          // reload the board when the move crosses the Done boundary.
          // KF-371: a drop into a sorted column reloads to its sorted place.
          var crossesDone = (toList.closest('[data-done-column="true"]') != null) ||
                            (fromList.closest('[data-done-column="true"]') != null) ||
                            toList.dataset.sorted === '1';
          fetch('/api/tasks/' + encodeURIComponent(card.dataset.taskId) + '/move?force=true', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              column_id: toCol,
              swimlane_id: toList.dataset.swimlaneId || null,
              position: position,
            }),
          }).then(function (res) {
            if (!res.ok) {
              if (res.status === 409) return refuseMove(res, card, fromList, evt.oldDraggableIndex);
              if (res.status === 403 || res.status === 422) {
                return toastRefusal(res, 'Could not move task.').then(function () {
                  setTimeout(function () { window.location.reload(); }, 2500);
                  return null;
                });
              }
              window.location.reload();
              return null;
            }
            return res.text();
          }).then(function (html) {
            if (html == null) return;
            if (crossesDone) { window.location.reload(); return; }
            // Swap in the refreshed card (Done stamp, totals) in place.
            var tmp = document.createElement('div');
            tmp.innerHTML = html;
            var fresh = tmp.firstElementChild;
            if (fresh) card.replaceWith(fresh);
            if (window.BoardChrome && BoardChrome.refold) BoardChrome.refold();
            paintSelection();
            // KF-199: the server-rendered card carries no client-side timer
            // state — re-apply the timer outline and live badge immediately
            // so a move while the timer runs re-renders at once instead of
            // waiting for the next 15s status poll.
            if (typeof TimerUI !== 'undefined') {
              if (TimerUI.updateCardIndicators) TimerUI.updateCardIndicators();
              if (TimerUI.updateCardLive) TimerUI.updateCardLive();
            }
            if (window.BoardChrome && BoardChrome.syncEpicFunnels) BoardChrome.syncEpicFunnels();
            if (card.dataset.kind === CARD_KIND.EPIC && window.BoardChrome) BoardChrome.refreshMarks();
            if (fromCol !== toCol && card.dataset.kind !== CARD_KIND.EPIC) {
              updateColumnCount(fromCol, -1, -estimate);
              updateColumnCount(toCol, 1, estimate);
            }
            refreshLaneCounts();
          }).catch(function () { window.location.reload(); });
        },
      });
    });
  }
  const putCardBack = (card, list, index) => {
    const rest = [...list.querySelectorAll('.task-card')].filter((el) => el !== card);
    if (rest[index]) list.insertBefore(card, rest[index]);
    else if (rest.length) rest[rest.length - 1].after(card);
    else list.appendChild(card);
  };

  const refuseMove = (res, card, list, index) => toastRefusal(res, 'Could not move task.').then(() => {
    putCardBack(card, list, index);
    if (window.BoardChrome && BoardChrome.refold) BoardChrome.refold();
    return null;
  });

  // Note: htmx inserts new cards inside the existing .task-list containers,
  // which already have Sortable attached, so no re-init is needed.

  // KF-441: where the pointer was last seen during a card drag.
  var lastDragPoint = null;
  ['dragover', 'pointermove', 'mousemove'].forEach(function (type) {
    document.addEventListener(type, function (e) { lastDragPoint = { x: e.clientX, y: e.clientY }; }, true);
  });

  document.addEventListener('dragover', function (e) {
    if (window.BoardChrome && BoardChrome.foldDragOver) BoardChrome.foldDragOver(e.target);
  }, true);

  function droppedOnCollapsedStrip() {
    if (!lastDragPoint) return false;
    var el = document.elementFromPoint(lastDragPoint.x, lastDragPoint.y);
    return !!(el && el.closest('td.board-cell--collapsed, th.columnHeader--collapsed'));
  }

  // KF-371: a card's hour estimate in minutes (0 when unset).
  function cardEstimateMinutes(card) {
    return card ? parseInt(card.dataset.estimateMinutes, 10) || 0 : 0;
  }

  // KF-371: an "Estimated hours" column sum; mirrors format_column_hours
  // in src/routes.rs.
  function formatColumnHours(minutes) {
    if (!(minutes > 0)) return '0h';
    var h = Math.floor(minutes / 60);
    var m = minutes % 60;
    if (h === 0) return m + 'm';
    return m === 0 ? h + 'h' : h + 'h ' + m + 'm';
  }

  function wipLimitState(th, count, minutes) {
    var wip = th.dataset.wipLimit ? parseInt(th.dataset.wipLimit, 10) : null;
    var hours = th.dataset.wipUnit === 'hours';
    return {
      label: !wip ? String(count) : (hours ? formatColumnHours(minutes) + ' / ' + wip + 'h' : count + ' / ' + wip),
      warn: wip != null && (hours ? minutes > wip * 60 : count > wip),
    };
  }

  // KF-208/KF-371: the right-edge badge shows the count, or the estimated
  // hours for Column sum "Estimated hours".
  function setSumBadge(th, count, minutes) {
    var badge = th.querySelector('.columnHeader-countBadge');
    if (!badge) return;
    badge.textContent = badge.dataset.columnSum === 'hours' ? formatColumnHours(minutes) : String(count);
  }

  const refreshGroupCounts = () => {
    const headers = document.querySelectorAll('.columnHeader[data-group-id]');
    const totals = new Map();
    headers.forEach((th) => {
      const id = th.dataset.groupId;
      totals.set(id, (totals.get(id) || 0) + (parseInt(th.dataset.taskCount, 10) || 0));
    });
    headers.forEach((th) => {
      const el = th.querySelector('.columnHeader-groupCount');
      if (!el) return;
      const count = totals.get(th.dataset.groupId);
      const limit = parseInt(th.dataset.groupLimit, 10);
      el.textContent = count + ' / ' + limit;
      el.classList.toggle('columnHeader-groupCount--over', count > limit);
    });
  };

  // Shift a column header's count by `delta` tasks and its estimate total
  // by `minutes`.
  function updateColumnCount(colId, delta, minutes) {
    var th = document.querySelector('.columnHeader[data-column-id="' + cssEscape(colId) + '"]');
    if (!th) return;
    if (window.BoardChrome && BoardChrome.refreshMarks) BoardChrome.refreshMarks();
    var count = (parseInt(th.dataset.taskCount, 10) || 0) + delta;
    th.dataset.taskCount = String(count);
    var estimate = (parseInt(th.dataset.estimateMinutes, 10) || 0) + (minutes || 0);
    th.dataset.estimateMinutes = String(estimate);
    refreshGroupCounts();
    // KF-161: if a filter is active, the header shows the visible subset —
    // recompute it rather than stamping the true total.
    if (window.BoardChrome && BoardChrome.isFiltering && BoardChrome.isFiltering()) {
      BoardChrome.refreshFilteredCounts();
      return;
    }
    var limit = wipLimitState(th, count, estimate);
    var el = th.querySelector('.columnHeader-count');
    if (el) el.textContent = limit.label;
    refreshLaneCounts();
    setSumBadge(th, count, estimate);
    var badge = th.querySelector('.columnHeader-countBadge');
    // KF-035: the warning is a *violation* — it fires only when the count
    // exceeds the limit, not when it merely reaches it.
    var warn = limit.warn;
    if (badge) {
      // KF-241: the collapsed strip's violated count must toggle the darkred
      // --over class along with the warning classes below.
      badge.classList.toggle('columnHeader-countBadge--over', warn);
    }
    th.classList.toggle('columnHeader--warning', warn);
    th.classList.toggle('columnHeader--limitWarning', warn);
    syncWarningLine(th, warn);
  }

  // KF-223/KF-362: overdue tasks also keep the line.
  function syncWarningLine(th, warn, overdue) {
    if (overdue == null) overdue = parseInt(th.dataset.overdueCount, 10) || 0;
    var show = warn || overdue > 0;
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

  var selectedIds = [];
  var selectionAnchor = null;

  function boardCardOrder() {
    var order = [];
    var headers = document.querySelectorAll('#column-headers-row .columnHeader');
    Array.prototype.forEach.call(headers, function (th, col) {
      var lists = document.querySelectorAll('.task-list[data-column-id="' + cssEscape(th.dataset.columnId) + '"]');
      Array.prototype.forEach.call(lists, function (list) {
        var index = 0;
        Array.prototype.forEach.call(list.querySelectorAll('.task-card'), function (el) {
          if (el.hidden || el.offsetParent === null) return;
          order.push({ id: el.dataset.taskId, el: el, col: col, lane: list.dataset.swimlaneId || '', index: index++ });
        });
      });
    });
    return order;
  }

  function rangeIds(order, fromId, toId) {
    var ids = order.map(function (entry) { return entry.id; });
    var a = ids.indexOf(fromId);
    var b = ids.indexOf(toId);
    if (a < 0 || b < 0) return b < 0 ? [] : [toId];
    return ids.slice(Math.min(a, b), Math.max(a, b) + 1);
  }

  function neighbourId(order, id, key) {
    var from = order.filter(function (entry) { return entry.id === id; })[0];
    if (!from) return null;
    var cell = function (entry, col) { return entry.col === col && entry.lane === from.lane; };
    if (key === 'ArrowUp' || key === 'ArrowDown') {
      var step = key === 'ArrowUp' ? -1 : 1;
      var same = order.filter(function (entry) { return cell(entry, from.col); });
      var next = same.filter(function (entry) { return entry.index === from.index + step; })[0];
      return next ? next.id : null;
    }
    var dir = key === 'ArrowLeft' ? -1 : 1;
    var cols = order.filter(function (entry) { return entry.lane === from.lane && (entry.col - from.col) * dir > 0; })
      .map(function (entry) { return entry.col; });
    if (!cols.length) return null;
    var target = cols.reduce(function (best, col) { return Math.abs(col - from.col) < Math.abs(best - from.col) ? col : best; });
    var cards = order.filter(function (entry) { return cell(entry, target); });
    return cards[Math.min(from.index, cards.length - 1)].id;
  }

  function isSelected(card) {
    return !!card && selectedIds.indexOf(card.dataset.taskId) !== -1;
  }

  function paintSelection() {
    document.querySelectorAll('.task-card').forEach(function (card) {
      card.classList.toggle('task-card--selected', isSelected(card));
    });
  }

  function setSelection(ids, anchor) {
    selectedIds = ids;
    if (anchor !== undefined) selectionAnchor = anchor;
    paintSelection();
  }

  function clearSelection() {
    setSelection([], null);
  }

  function selectedCards() {
    return boardCardOrder().filter(function (entry) { return selectedIds.indexOf(entry.id) !== -1; })
      .map(function (entry) { return entry.el; });
  }

  function dragGroupIds(card) {
    if (!isSelected(card)) return null;
    var cards = selectedCards();
    if (cards.length < 2) return null;
    return cards.map(function (el) { return el.dataset.taskId; });
  }

  function dragHelper(card) {
    var count = isSelected(card) ? selectedCards().length : 1;
    var helper = document.createElement('div');
    helper.className = 'drag-helper' + (count > 1 ? ' drag-helper--group' : '');
    helper.textContent = count > 1 ? 'Move ' + count + ' tasks' : (card.dataset.taskName || '');
    document.body.appendChild(helper);
    window.setTimeout(function () { helper.remove(); }, 0);
    return helper;
  }

  function markDoneTarget(list) {
    document.querySelectorAll('.done-group-label--target').forEach(function (label) {
      label.classList.remove('done-group-label--target');
    });
    if (!list || list.dataset.doneColumn !== 'true') return;
    Array.prototype.some.call(list.querySelectorAll('.done-group-label'), function (label) {
      if (label.textContent.trim() !== 'Today') return false;
      label.classList.add('done-group-label--target');
      return true;
    });
  }

  function reloadSelectionSoon(failed) {
    if (failed) setTimeout(function () { window.location.reload(); }, 2500);
    else window.location.reload();
  }

  function moveSelectionTo(ids, toList, position) {
    var body = {
      task_ids: ids,
      action: 'move',
      column_id: toList.dataset.columnId,
      swimlane_id: toList.dataset.swimlaneId || null,
      position: position,
    };
    api('/api/boards/' + encodeURIComponent(boardId()) + '/tasks/bulk', 'POST', body)
      .then(drained)
      .then(function (res) {
        if (!res.ok) toast('Could not move the selected tasks.');
        reloadSelectionSoon(!res.ok);
      })
      .catch(function () {
        toast('Could not move the selected tasks.');
        reloadSelectionSoon(true);
      });
  }

  function deleteSelection() {
    var cards = selectedCards();
    if (!cards.length) return;
    var title = cards.length === 1 ? 'Delete task' : 'Delete tasks';
    var message = cards.length === 1
      ? 'Delete "' + (cards[0].dataset.taskName || '') + '"?'
      : 'Delete ' + cards.length + ' tasks?';
    showConfirmDialog(title, message, 'Delete', function () {
      Promise.all(cards.map(function (card) {
        return api('/api/tasks/' + encodeURIComponent(card.dataset.taskId), 'DELETE').then(drained);
      })).then(function (results) {
        var failed = results.filter(function (res) { return !res.ok; }).length;
        if (failed) toast('Could not delete ' + failed + (failed === 1 ? ' task.' : ' tasks.'));
        reloadSelectionSoon(failed > 0);
      }).catch(function () {
        toast('Could not delete tasks.');
        reloadSelectionSoon(true);
      });
    });
  }

  function postBulk(body) {
    return api('/api/boards/' + encodeURIComponent(boardId()) + '/tasks/bulk', 'POST', body).then(drained);
  }

  function refreshBulkCards(ids) {
    if (typeof TimerUI !== 'undefined' && TimerUI.refreshTaskCard) ids.forEach(TimerUI.refreshTaskCard, TimerUI);
  }

  function bulkColor(ids, colorId) {
    return postBulk({ task_ids: ids, action: 'color', color_id: colorId }).then(function (res) {
      if (!res.ok) { toast('Could not change color.'); return; }
      refreshBulkCards(ids);
    });
  }

  function bulkUnassign(ids) {
    return postBulk({ task_ids: ids, action: 'unassign_members' }).then(function (res) {
      if (!res.ok) { toast('Could not update members.'); return; }
      refreshBulkCards(ids);
    });
  }

  function commonMembers(ids) {
    var lists = ids.map(function (id) {
      var card = document.querySelector('.task-card[data-task-id="' + cssEscape(id) + '"]');
      return card && card.dataset.memberIds ? card.dataset.memberIds.split(',') : [];
    });
    return lists.length ? lists[0].filter(function (member) {
      return lists.every(function (list) { return list.indexOf(member) !== -1; });
    }) : [];
  }

  function anchorPopover(overlay, anchor) {
    var dlg = overlay.querySelector('.dlg');
    overlay.classList.toggle('dlg-overlay--popover', !!anchor);
    dlg.style.left = '';
    dlg.style.top = '';
    overlay.hidden = false;
    if (!anchor) return;
    var r = anchor.getBoundingClientRect();
    var w = dlg.offsetWidth;
    var h = dlg.offsetHeight;
    var top = r.bottom + 4 + h <= window.innerHeight - 8 ? r.bottom + 4 : r.top - h - 4;
    dlg.style.left = Math.max(8, Math.min(r.left, window.innerWidth - w - 8)) + 'px';
    dlg.style.top = Math.max(8, Math.min(top, window.innerHeight - h - 8)) + 'px';
  }

  function taskAnchor(taskId, modalSelector) {
    if (!taskId) return null;
    if (modalSelector && modalTaskId() === taskId) return document.querySelector(modalSelector);
    return document.querySelector('.task-card[data-task-id="' + cssEscape(taskId) + '"]');
  }

  function openBulkMembersDialog(ids) {
    initMembersDialog();
    membersBulkIds = ids;
    membersDialogTaskId = null;
    membersRoster = [];
    membersAssigned = commonMembers(ids);
    membersResponsible = null;
    var search = document.getElementById('members-search');
    search.value = '';
    document.getElementById('members-list').innerHTML = '<div class="member-row">Loading…</div>';
    anchorPopover(document.getElementById('members-dialog'), taskAnchor(ids[0], ''));
    search.focus();
    fetchJson('/api/members').catch(function () { return []; }).then(function (roster) {
      if (membersBulkIds !== ids) return;
      membersRoster = roster || [];
      renderMembersList('');
    });
  }

  function toggleBulkMember(memberId) {
    var ids = membersBulkIds;
    var everyone = membersAssigned.indexOf(memberId) !== -1;
    postBulk({
      task_ids: ids,
      action: 'members',
      add_member_ids: everyone ? [] : [memberId],
      remove_member_ids: everyone ? [memberId] : [],
    }).then(function (res) {
      if (!res.ok) { toast('Could not update members.'); return; }
      membersAssigned = everyone
        ? membersAssigned.filter(function (member) { return member !== memberId; })
        : membersAssigned.concat([memberId]);
      renderMembersList(document.getElementById('members-search').value);
      refreshBulkCards(ids);
    });
  }

  function typingTarget(el) {
    return !!el && (/^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName) || el.isContentEditable === true);
  }

  function selectionBlocked() {
    return !!document.querySelector('#modal-root .task-modal, .dlg-overlay:not([hidden]), .menu-floating:not([hidden])');
  }

  function onSelectionKey(e) {
    if (!selectedIds.length || e.defaultPrevented || typingTarget(e.target) || selectionBlocked()) return;
    if (e.altKey || e.ctrlKey || e.metaKey) return;
    if (e.key === 'Escape') {
      clearSelection();
    } else if (/^Arrow(Up|Down|Left|Right)$/.test(e.key) && !e.shiftKey) {
      e.preventDefault();
      var order = boardCardOrder();
      var from = selectionAnchor && selectedIds.indexOf(selectionAnchor) !== -1 ? selectionAnchor : selectedIds[selectedIds.length - 1];
      var next = neighbourId(order, from, e.key);
      if (!next) return;
      setSelection([next], next);
      var entry = order.filter(function (item) { return item.id === next; })[0];
      if (entry) {
        entry.el.scrollIntoView({ block: 'nearest', inline: 'nearest' });
        entry.el.focus({ preventScroll: true });
      }
    } else if (e.key === 'Delete' || e.key === 'Backspace') {
      e.preventDefault();
      deleteSelection();
    }
  }

  function onSelectionClick(e) {
    if (!(e.metaKey || e.ctrlKey || e.shiftKey) || dragging) return;
    var card = e.target.closest ? e.target.closest('.task-card') : null;
    if (!card || !card.dataset.taskId || e.target.closest('button, a, input, select, textarea, label')) return;
    e.preventDefault();
    e.stopPropagation();
    var id = card.dataset.taskId;
    if (e.shiftKey && selectionAnchor) {
      var range = rangeIds(boardCardOrder(), selectionAnchor, id);
      var kept = (e.metaKey || e.ctrlKey) ? selectedIds.filter(function (other) { return range.indexOf(other) === -1; }) : [];
      setSelection(kept.concat(range));
    } else if (e.shiftKey) {
      setSelection([id], id);
    } else if (isSelected(card)) {
      setSelection(selectedIds.filter(function (other) { return other !== id; }), id);
    } else {
      setSelection(selectedIds.concat([id]), id);
    }
  }

  function initSelection() {
    document.addEventListener('click', onSelectionClick, true);
    document.addEventListener('keydown', onSelectionKey);
    document.addEventListener('mousedown', function (e) {
      if (e.shiftKey && e.target.closest && e.target.closest('.task-card')) e.preventDefault();
    });
    document.addEventListener('htmx:afterSwap', paintSelection);
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
          .then(drained)
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
        forceFallback: true,
        filter: 'button',
        preventOnFilter: false,
        ghostClass: 'sortable-placeholder',
        onEnd: function (evt) {
          var items = Array.prototype.slice.call(colList.querySelectorAll('[data-column-id]'));
          var newIndex = items.findIndex(function (li) { return li.dataset.columnId === evt.item.dataset.columnId; });
          api('/api/columns/' + encodeURIComponent(evt.item.dataset.columnId) + '/move', 'POST', { position: newIndex })
            .then(drained)
            .then(function (res) { if (res.ok) window.location.reload(); else toast('Could not reorder columns.'); })
            .catch(function () { toast('Could not reorder columns.'); });
        },
      });
    }
    var laneList = document.getElementById('layout-swimlanes-list');
    if (laneList && !laneList._sortable) {
      laneList._sortable = new Sortable(laneList, {
        animation: 150,
        forceFallback: true,
        filter: 'button',
        preventOnFilter: false,
        ghostClass: 'sortable-placeholder',
        onEnd: function (evt) {
          var items = Array.prototype.slice.call(laneList.querySelectorAll('[data-swimlane-id]'));
          var newIndex = items.findIndex(function (li) { return li.dataset.swimlaneId === evt.item.dataset.swimlaneId; });
          api('/api/swimlanes/' + encodeURIComponent(evt.item.dataset.swimlaneId) + '/move', 'POST', { position: newIndex })
            .then(drained)
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

  // KF-208: clamp a fixed-position popup so it always opens fully in view.
  function placePopup(popup, x, y) {
    popup.style.left = '0px';
    popup.style.top = '0px';
    var w = popup.offsetWidth;
    var h = popup.offsetHeight;
    popup.style.left = Math.max(4, Math.min(x, window.innerWidth - w - 4)) + 'px';
    popup.style.top = Math.max(4, Math.min(y, window.innerHeight - h - 4)) + 'px';
  }

  // ---------- KF-302: add-task dialog ----------

  // KanbanFlow's column "+" opens one centred dialog tinted with the chosen
  // colour: name, colour select, shortcuts row, and three save actions.
  function initAddTask() {
    var overlay = document.getElementById('add-task-overlay');
    if (!overlay) return;
    var form = document.getElementById('add-task-dialog');
    var nameInput = form.elements.name;
    var selectBtn = form.querySelector('.addTaskDialog-colorSelect');
    var menu = form.querySelector('.colorSelectMenu');
    var busy = false;
    var membersPop = document.getElementById('add-members-pop');
    var membersBtn = form.querySelector('[data-add-members]');
    var membersSearch = membersPop.querySelector('.addMembersPop-search');
    var membersList = membersPop.querySelector('.addMembersPop-list');
    var roster = null;
    var picked = [];
    function defaultMembers() {
      var wrap = document.querySelector('.board-wrap');
      var raw = wrap ? wrap.dataset.defaultMembers : '';
      return raw ? raw.split(',') : [];
    }
    function sameMembers(a, b) {
      return a.length === b.length && a.every(function (id) { return b.indexOf(id) !== -1; });
    }

    // KF-481: the Members button opens a popover inside the dialog; the
    // picked members are assigned once the task exists.
    function renderMembers() {
      var q = membersSearch.value.trim().toLowerCase();
      membersList.innerHTML = '';
      (roster || []).forEach(function (m) {
        if (q && m.display_name.toLowerCase().indexOf(q) === -1) return;
        var row = document.createElement('label');
        row.className = 'addMembersPop-row';
        var box = document.createElement('input');
        box.type = 'checkbox';
        box.checked = picked.indexOf(m.id) !== -1;
        box.addEventListener('change', function () {
          var at = picked.indexOf(m.id);
          if (box.checked && at === -1) picked.push(m.id);
          else if (!box.checked && at !== -1) picked.splice(at, 1);
        });
        var avatar = document.createElement('span');
        avatar.className = 'addMembersPop-avatar';
        paintAvatar(avatar, m);
        var name = document.createElement('span');
        name.className = 'addMembersPop-name';
        name.textContent = m.display_name;
        row.appendChild(box);
        row.appendChild(avatar);
        row.appendChild(name);
        membersList.appendChild(row);
      });
    }

    function closeMembers() {
      // Hiding the focused search field would drop focus to <body>, where
      // the dialog no longer sees Escape.
      if (membersPop.contains(document.activeElement)) nameInput.focus();
      membersPop.hidden = true;
      membersBtn.setAttribute('aria-expanded', 'false');
    }

    function openMembers() {
      membersPop.hidden = false;
      membersBtn.setAttribute('aria-expanded', 'true');
      membersSearch.value = '';
      if (roster) { renderMembers(); membersSearch.focus(); return; }
      fetchJson('/api/members').catch(function () { return []; }).then(function (list) {
        roster = list || [];
        renderMembers();
      });
      membersSearch.focus();
    }

    membersSearch.addEventListener('input', renderMembers);

    var labelsPop = document.getElementById('add-labels-pop');
    var labelsBtn = form.querySelector('[data-add-labels]');
    var labelsInput = labelsPop.querySelector('.addLabelsPop-input');
    var labelsChosen = labelsPop.querySelector('.addLabelsPop-chosen');
    var labelsSugg = labelsPop.querySelector('.addLabelsPop-sugg');
    var labelSuggestions = null;
    var chosenLabels = [];
    var descBtn = form.querySelector('[data-add-desc]');
    var descSection = document.getElementById('add-desc-section');
    var descText = descSection.querySelector('.addDesc-text');
    var descPreview = descSection.querySelector('.addDesc-preview');
    var moreBtn = form.querySelector('[data-add-more]');
    var moreMenu = document.getElementById('add-more-menu');

    // KF-481: the Labels button opens a popover; the chosen labels are sent
    // with the task.
    function renderLabels() {
      labelsChosen.innerHTML = '';
      chosenLabels.forEach(function (l) {
        var chip = document.createElement('span');
        chip.className = 'tm-label-chip';
        chip.textContent = l + ' ';
        var x = document.createElement('button');
        x.type = 'button';
        x.className = 'tm-label-x';
        x.textContent = '×';
        x.setAttribute('aria-label', 'Remove label ' + l);
        x.addEventListener('click', function () {
          chosenLabels = chosenLabels.filter(function (c) { return c !== l; });
          renderLabels();
        });
        chip.appendChild(x);
        labelsChosen.appendChild(chip);
      });
      labelsSugg.innerHTML = '';
      (labelSuggestions || []).filter(function (s) { return chosenLabels.indexOf(s) === -1; }).forEach(function (s) {
        var b = document.createElement('button');
        b.type = 'button';
        b.className = 'tm-label-chip tm-label-sug';
        b.textContent = s;
        b.addEventListener('click', function () { addLabel(s); });
        labelsSugg.appendChild(b);
      });
      if (!labelsSugg.children.length) {
        labelsSugg.innerHTML = '<span class="empty-note">No recently used labels exist</span>';
      }
    }

    function addLabel(raw) {
      var l = (raw || '').trim();
      if (l && chosenLabels.indexOf(l) === -1) chosenLabels.push(l);
      labelsInput.value = '';
      renderLabels();
      labelsInput.focus();
    }

    function closeLabels() {
      if (labelsPop.contains(document.activeElement)) nameInput.focus();
      labelsPop.hidden = true;
      labelsBtn.setAttribute('aria-expanded', 'false');
    }

    function openLabels() {
      closeMembers();
      closeMore();
      labelsPop.hidden = false;
      labelsBtn.setAttribute('aria-expanded', 'true');
      labelsSugg.hidden = true;
      labelsInput.value = '';
      renderLabels();
      labelsInput.focus();
      if (labelSuggestions) return;
      api('/api/boards/' + encodeURIComponent(boardId()) + '/labels', 'GET')
        .then(function (res) { return res.ok ? res.json() : []; })
        .catch(function () { return []; })
        .then(function (list) { labelSuggestions = list || []; renderLabels(); });
    }

    var descPreviewSeq = 0;
    function renderDescPreview() {
      var text = descText.value;
      var seq = ++descPreviewSeq;
      if (!text.trim()) {
        descPreview.textContent = 'Nothing to preview';
        return;
      }
      descPreview.textContent = '';
      api('/api/markdown/preview', 'POST', { description: text })
        .then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); })
        .then(function (html) {
          if (seq !== descPreviewSeq) return;
          var body = document.createElement('div');
          body.className = 'tm-md';
          body.innerHTML = html;
          descPreview.textContent = '';
          descPreview.appendChild(body);
        })
        .catch(function (status) {
          if (seq !== descPreviewSeq) return;
          descPreview.textContent = status === 400
            ? 'The description is too long to preview'
            : 'The preview could not be loaded';
        });
    }

    function setDescTab(tab) {
      var preview = tab === 'preview';
      descSection.querySelectorAll('[data-add-desc-tab]').forEach(function (t) {
        t.classList.toggle('addDesc-tab--on', t.getAttribute('data-add-desc-tab') === tab);
      });
      descText.hidden = preview;
      descPreview.hidden = !preview;
      if (preview) renderDescPreview();
      else descText.focus();
    }

    function openDesc() {
      descSection.hidden = false;
      descBtn.classList.add('addTaskDialog-iconBtn--on');
      descBtn.setAttribute('aria-expanded', 'true');
      setDescTab('write');
    }

    function closeDesc() {
      descSection.hidden = true;
      descBtn.classList.remove('addTaskDialog-iconBtn--on');
      descBtn.setAttribute('aria-expanded', 'false');
    }

    function wrapDesc(mark) {
      var a = descText.selectionStart;
      var b = descText.selectionEnd;
      var sel = descText.value.slice(a, b);
      var out = mark === 'link' ? '[' + sel + '](url)' : mark + sel + mark;
      descText.setRangeText(out, a, b, 'end');
      descText.focus();
    }

    function closeMore() {
      moreMenu.hidden = true;
      moreBtn.setAttribute('aria-expanded', 'false');
    }

    function openMore() {
      closeMembers();
      closeLabels();
      moreMenu.hidden = false;
      var r = moreBtn.getBoundingClientRect();
      var f = form.getBoundingClientRect();
      moreMenu.style.left = Math.max(0, r.right - f.left - moreMenu.offsetWidth) + 'px';
      moreMenu.style.top = (r.bottom - f.top + 2) + 'px';
      moreBtn.setAttribute('aria-expanded', 'true');
    }

    function colorSpans() {
      return Array.prototype.slice.call(document.querySelectorAll('#board-colors span[data-id]'));
    }

    function setColor(span) {
      form.elements.color_id.value = span ? span.dataset.id : '';
      form.style.backgroundColor = span ? span.dataset.bg : '';
      form.style.setProperty('--addTaskBorder', span ? span.dataset.border : '#d4d4d4');
      form.querySelector('.addTaskDialog-colorName').textContent =
        span ? (span.dataset.label || span.dataset.value) : '';
    }

    function fitName() {
      nameInput.style.height = 'auto';
      nameInput.style.height = nameInput.scrollHeight + 'px';
    }

    function closeMenu() {
      menu.hidden = true;
      selectBtn.setAttribute('aria-expanded', 'false');
    }

    function renameColor(span, text) {
      var label = text.value.trim();
      if (!label || label === (span.dataset.label || span.dataset.value)) { openMenu(); return; }
      api('/api/boards/' + encodeURIComponent(boardId()) + '/colors/' + encodeURIComponent(span.dataset.id),
          'PATCH', { label: label })
        .then(function (res) {
          if (!res.ok) { toast('Could not rename the color.'); return; }
          span.dataset.label = label;
          var segments = document.querySelectorAll('.color-legend-segment');
          var at = colorSpans().indexOf(span);
          if (segments.length === colorSpans().length && at >= 0) segments[at].textContent = label;
          if (form.elements.color_id.value === span.dataset.id) setColor(span);
          openMenu();
        });
    }

    function startColorRename(span, opt, text) {
      var input = document.createElement('input');
      input.type = 'text';
      input.className = 'colorSelectMenu-rename';
      input.maxLength = 50;
      input.value = span.dataset.label || span.dataset.value;
      opt.replaceChild(input, text);
      opt.querySelector('.colorSelectMenu-kebab').hidden = true;
      input.focus();
      input.select();
      input.addEventListener('click', function (e) { e.stopPropagation(); });
      input.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') { e.preventDefault(); e.stopPropagation(); renameColor(span, input); }
        else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); openMenu(); }
      });
    }

    function colorKebab(span, opt, text) {
      var kebab = document.createElement('button');
      kebab.type = 'button';
      kebab.className = 'colorSelectMenu-kebab';
      kebab.setAttribute('aria-label', 'Color options');
      kebab.setAttribute('aria-haspopup', 'menu');
      kebab.innerHTML = '<svg class="ico-kebab" width="4" height="16" viewBox="0 0 4 16" fill="currentColor" aria-hidden="true" focusable="false"><circle cx="2" cy="2.5" r="1.5"/><circle cx="2" cy="8" r="1.5"/><circle cx="2" cy="13.5" r="1.5"/></svg>';
      var actions = document.createElement('div');
      actions.className = 'colorSelectMenu-actions';
      actions.setAttribute('role', 'menu');
      actions.hidden = true;
      [['Select', function () { setColor(span); closeMenu(); nameInput.focus(); }],
       ['Rename', function () { startColorRename(span, opt, text); }]].forEach(function (item) {
        var b = document.createElement('button');
        b.type = 'button';
        b.setAttribute('role', 'menuitem');
        b.textContent = item[0];
        b.addEventListener('click', function (e) { e.stopPropagation(); item[1](); });
        actions.appendChild(b);
      });
      kebab.addEventListener('click', function (e) {
        e.stopPropagation();
        var wasHidden = actions.hidden;
        menu.querySelectorAll('.colorSelectMenu-actions').forEach(function (a) { a.hidden = true; });
        actions.hidden = !wasHidden;
      });
      opt.appendChild(kebab);
      opt.appendChild(actions);
    }

    function openMenu() {
      menu.innerHTML = '';
      colorSpans().forEach(function (span) {
        var opt = document.createElement('div');
        opt.className = 'colorSelectMenu-option';
        opt.setAttribute('role', 'option');
        opt.dataset.colorId = span.dataset.id;
        if (span.dataset.id === form.elements.color_id.value) {
          opt.classList.add('colorSelectMenu-option--selected');
          opt.setAttribute('aria-selected', 'true');
        }
        var dot = document.createElement('span');
        dot.className = 'colorSelectMenu-dot';
        dot.style.backgroundColor = span.dataset.border;
        var text = document.createElement('span');
        text.textContent = span.dataset.label || span.dataset.value;
        opt.appendChild(dot);
        opt.appendChild(text);
        colorKebab(span, opt, text);
        opt.addEventListener('click', function () {
          setColor(span);
          closeMenu();
          nameInput.focus();
        });
        menu.appendChild(opt);
      });
      menu.hidden = false;
      selectBtn.setAttribute('aria-expanded', 'true');
    }

    function openList(colId, laneId) {
      var lists = document.querySelectorAll('.task-list[data-column-id="' + cssEscape(colId) + '"]');
      var open = null;
      lists.forEach(function (list) {
        if (open) return;
        var inLane = !laneId || list.dataset.swimlaneId === laneId;
        if (inLane && !list.closest('tr.swimlane-row--folded')) open = list;
      });
      return open || lists[0] || null;
    }

    function open(colId, laneId) {
      hideFloatingMenus();
      form.reset();
      form.elements.column_id.value = colId;
      var firstList = openList(colId, laneId);
      form.elements.swimlane_id.value = firstList ? (firstList.dataset.swimlaneId || '') : '';
      var spans = colorSpans();
      setColor(spans.filter(function (s) { return s.dataset.default === 'true'; })[0] || spans[0] || null);
      closeMenu();
      closeMembers();
      closeLabels();
      closeMore();
      closeDesc();
      chosenLabels = [];
      picked = defaultMembers();
      overlay.hidden = false;
      fitName();
      nameInput.focus();
    }

    function close() {
      closeMenu();
      closeMembers();
      closeLabels();
      closeMore();
      overlay.hidden = true;
    }

    function insertCard(colId, html) {
      var tpl = document.createElement('template');
      tpl.innerHTML = html.trim();
      var card = tpl.content.firstElementChild;
      // Insert into the column's first visible task-list (unhiding per
      // KF-206 when every lane is folded).
      var list = openList(colId, form.elements.swimlane_id.value);
      if (list && card) {
        list.removeAttribute('hidden');
        // KF-403: the board's "Position of new tasks", bottom by default.
        var wrap = document.querySelector('.board-wrap');
        if (wrap && wrap.dataset.newTasksAtTop === '1') {
          list.insertBefore(card, list.querySelector('.task-card'));
        } else {
          list.appendChild(card);
        }
        if (window.htmx) window.htmx.process(card);
        if (window.BoardChrome && BoardChrome.refold) BoardChrome.refold();
        refreshCardSecStates(); // KF-224: new card honors section state
      }
      updateColumnCount(colId, 1, cardEstimateMinutes(card));
      return card;
    }

    // mode: 'more' keeps the dialog open, 'open' opens the new task,
    // 'close' just closes.
    // follow: 'duedate' / 'estimate' open that control in the new task's details.
    function save(mode, follow) {
      if (busy) return;
      if (!nameInput.value.trim()) { nameInput.focus(); return; }
      busy = true;
      var colId = form.elements.column_id.value;
      var body = {
        column_id: colId,
        swimlane_id: form.elements.swimlane_id.value,
        color_id: form.elements.color_id.value,
        name: nameInput.value,
        description: descText.value,
        labels: chosenLabels.slice(),
      };
      if (!sameMembers(picked, defaultMembers())) body.member_ids = picked.slice();
      fetch('/api/tasks', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
        credentials: 'same-origin',
      })
        .then(function (resp) {
          if (!resp.ok) {
            return resp.text().then(function (t) {
              var message = t;
              try { message = JSON.parse(t).message || t; } catch (e) {}
              throw new Error(message || ('HTTP ' + resp.status));
            });
          }
          return resp.text();
        })
        .then(function (html) {
          busy = false;
          var card = insertCard(colId, html);
          var taskId = card && card.dataset.taskId;
          if (mode === 'more') {
            closeMembers();
            closeLabels();
            closeMore();
            closeDesc();
            descText.value = '';
            chosenLabels = [];
            picked = defaultMembers();
            nameInput.value = '';
            fitName();
            nameInput.focus();
            return;
          }
          close();
          if (mode === 'open' && taskId) {
            openModal(taskId);
            if (follow) TimerUI.clickAfterModal('#tm-add-menu [data-tm-act="' + follow + '"]');
          }
        })
        .catch(function (err) {
          busy = false;
          toast('Could not add task: ' + (err.message || err));
        });
    }

    document.addEventListener('click', function (e) {
      var btn = e.target.closest('[data-add-task-for]');
      if (btn) open(btn.getAttribute('data-add-task-for'), btn.getAttribute('data-add-lane'));
    });

    document.addEventListener('dblclick', function (e) {
      var cell = e.target.closest('td.board-cell');
      if (!cell || cell.classList.contains('board-cell--collapsed')) return;
      if (e.target.closest('.task-card, button, a, input, select, textarea, .columnOverdue')) return;
      open(cell.dataset.columnId, cell.dataset.swimlaneId);
    });

    overlay.addEventListener('click', function (e) {
      if (e.target === overlay) { close(); return; }
      if (e.target.closest('[data-add-close]')) { close(); return; }
      if (e.target.closest('[data-add-members-close]')) { closeMembers(); membersBtn.focus(); return; }
      if (e.target.closest('[data-add-members]')) {
        if (membersPop.hidden) openMembers(); else closeMembers();
        return;
      }
      if (e.target.closest('[data-add-labels-close]')) { closeLabels(); labelsBtn.focus(); return; }
      if (e.target.closest('[data-add-labels]')) {
        if (labelsPop.hidden) openLabels(); else closeLabels();
        return;
      }
      if (e.target.closest('[data-add-labels-list]')) {
        labelsSugg.hidden = !labelsSugg.hidden;
        return;
      }
      if (e.target.closest('[data-add-desc]')) {
        if (descSection.hidden) openDesc(); else closeDesc();
        return;
      }
      var descTab = e.target.closest('[data-add-desc-tab]');
      if (descTab) { setDescTab(descTab.getAttribute('data-add-desc-tab')); return; }
      var fmt = e.target.closest('[data-add-desc-fmt]');
      if (fmt) { setDescTab('write'); wrapDesc(fmt.getAttribute('data-add-desc-fmt')); return; }
      if (e.target.closest('[data-add-more]')) {
        if (moreMenu.hidden) openMore(); else closeMore();
        return;
      }
      var moreAct = e.target.closest('[data-add-more-act]');
      if (moreAct) {
        closeMore();
        var act = moreAct.getAttribute('data-add-more-act');
        save('open', act === 'other' ? '' : act);
        return;
      }
      var saveBtn = e.target.closest('[data-add-save]');
      if (saveBtn) { save(saveBtn.getAttribute('data-add-save')); return; }
      if (e.target.closest('.addTaskDialog-colorSelect')) {
        if (menu.hidden) openMenu(); else closeMenu();
        return;
      }
      if (!e.target.closest('.colorSelectMenu')) closeMenu();
    });

    form.addEventListener('submit', function (e) { e.preventDefault(); save('close'); });
    nameInput.addEventListener('input', fitName);

    // Ctrl+Shift+Enter adds more, Shift+Enter opens details, Ctrl+Enter or
    // a plain Enter in the name closes; Escape closes the colour menu first.
    form.addEventListener('keydown', function (e) {
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        if (!moreMenu.hidden) closeMore();
        else if (!menu.hidden) closeMenu();
        else if (!membersPop.hidden) closeMembers();
        else if (!labelsPop.hidden) closeLabels();
        else close();
        return;
      }
      if (e.key !== 'Enter') return;
      // KF-481: Enter in the Members search only filters the list.
      if (e.target === membersSearch) { e.preventDefault(); return; }
      if (e.target === labelsInput) { e.preventDefault(); addLabel(labelsInput.value); return; }
      if (e.target === descText && !(e.ctrlKey || e.metaKey || e.shiftKey)) return;
      var ctrl = e.ctrlKey || e.metaKey;
      if (ctrl && e.shiftKey) { e.preventDefault(); save('more'); }
      else if (e.shiftKey) { e.preventDefault(); save('open'); }
      else if (ctrl || e.target === nameInput) { e.preventDefault(); save('close'); }
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
    ['notifications-btn', 'account-btn'].forEach(function (id) {
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
    var epicList = document.getElementById('tm-epic-list');
    if (epicList) epicList.hidden = true;
    var epicTrigger = document.querySelector('[data-tm-act="add-to-epic"]');
    if (epicTrigger) epicTrigger.setAttribute('aria-expanded', 'false');
    return any;
  }

  var colMenuColumnId = null;
  var laneMenuSwimlaneId = null;
  var layoutMenuColumnId = null;

  function openLaneMenu(id, mode, x, y) {
    var lanes = laneOrder();
    var at = lanes.findIndex(function (l) { return l.id === id; });
    if (at < 0) return;
    laneMenuSwimlaneId = id;
    var menu = document.getElementById('swimlane-menu');
    var inBoard = mode === 'board';
    function show(act, visible) { menu.querySelector('[data-lane-act="' + act + '"]').hidden = !visible; }
    show('edit', true);
    show('fold', inBoard);
    show('up', !inBoard && at > 0);
    show('down', !inBoard && at < lanes.length - 1);
    show('above', !inBoard);
    show('below', !inBoard);
    show('delete', !inBoard && lanes.length > 1);
    menu.querySelector('[data-lane-act="fold"]').textContent = lanes[at].collapsed ? 'Expand' : 'Collapse';
    placeMenu(menu, x, y);
  }

  function initBoardMenus() {
    // KF-038/KF-230: no ⋮ button on column headers (KanbanFlow parity) —
    // the column menu (Edit / Collapse / Show details / Delete) opens on
    // right-click. The lane ⋮ menu is unchanged.
    document.addEventListener('click', function (e) {
      var foldToggle = e.target.closest('[data-lane-fold-toggle]');
      if (foldToggle) {
        toggleLaneFold(foldToggle.getAttribute('data-lane-fold-toggle'));
        return;
      }
      if (e.target.closest('[data-lane-names-toggle]')) {
        var folded = e.target.closest('[data-lane-names-toggle]').getAttribute('aria-pressed') === 'true';
        reloadAfter(api('/api/boards/' + encodeURIComponent(boardId()) + '/config', 'PUT', { lane_names_folded: !folded }),
          'Could not update the board.');
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
        if (isSelected(card) && selectedCards().length > 1) openBulkMenu(e.clientX, e.clientY);
        else openCardMenu(card, e.clientX, e.clientY);
        return;
      }
      // KF-038/KF-230: no ⋮ button on column headers (KanbanFlow parity) —
      // the 4-item column menu (Edit / Collapse / Show details / Delete)
      // opens on right-click.
      var laneLabel = e.target.closest('th.swimlane-label');
      if (laneLabel) {
        e.preventDefault();
        openLaneMenu(laneLabel.dataset.swimlaneId, 'board', e.clientX, e.clientY);
        return;
      }
      var th = e.target.closest('.columnHeader');
      if (!th) return;
      e.preventDefault();
      colMenuColumnId = th.dataset.columnId;
      // KF-223: the Collapse item reads "Expand" for an already-collapsed column.
      var collapseBtn = document.getElementById('column-menu').querySelector('[data-col-act="collapse"]');
      if (collapseBtn) collapseBtn.textContent = th.dataset.collapsed === '1' ? 'Expand' : 'Collapse';
      var watchBtn = document.getElementById('column-menu').querySelector('[data-col-act="watch"]');
      if (watchBtn) watchBtn.textContent = th.dataset.watched === '1' ? 'Unwatch' : 'Watch';
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
      else if (act === 'watch') toggleColumnWatched(id);
    });

    function toggleColumnWatched(id) {
      var th = document.querySelector('.columnHeader[data-column-id="' + id + '"]');
      var watch = !(th && th.dataset.watched === '1');
      api('/api/columns/' + encodeURIComponent(id), 'PATCH', { watched: watch })
        .then(function (res) {
          return res.text().then(function () {
            if (res.ok) window.location.reload();
            else toast('Could not update the column.');
          });
        })
        .catch(function () { toast('Could not update the column.'); });
    }

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
      // KF-543: a click on an expanded column's name collapses it; a drag
      // starts from the same header but never fires this click.
      var nameEl = e.target.closest('.columnHeader-name');
      if (nameEl) {
        var owner = nameEl.closest('.columnHeader');
        if (owner) toggleColumnCollapsed(owner.dataset.columnId);
        return;
      }
      var th = e.target.closest('.columnHeader--collapsed');
      if (!th || e.target.closest('button')) return;
      toggleColumnCollapsed(th.dataset.columnId);
    });
    // KF-441: Enter and Space expand a focused strip.
    document.getElementById('column-headers-row').addEventListener('keydown', function (e) {
      if ((e.key !== 'Enter' && e.key !== ' ') || e.metaKey || e.ctrlKey || e.altKey) return;
      var th = e.target.closest('.columnHeader--collapsed');
      if (!th || e.target !== th) return;
      e.preventDefault();
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
      if (act === 'edit') openEditLaneDialog(id);
      else if (act === 'fold') toggleLaneFold(id);
      else if (act === 'up') moveSwimlane(id, -1);
      else if (act === 'down') moveSwimlane(id, 1);
      else if (act === 'above') addLaneBeside(id, 'above');
      else if (act === 'below') addLaneBeside(id, 'below');
      else if (act === 'delete') deleteSwimlane(id);
    });

    // ---------- card context menu (KF-050) ----------
    var cardMenuCard = null;
    var cardMenuPoint = { x: 0, y: 0 };
    var groupingDateTaskId = null;

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
      cardMenuPoint = { x: x, y: y };
      epicHoverTried = false;
      hoveredAct = null;
      var groupingDate = document.querySelector('#card-ctx-menu [data-card-act="grouping-date"]');
      if (groupingDate) groupingDate.hidden = !card.closest('[data-done-column="true"]');
      placeMenu(document.getElementById('card-ctx-menu'), x, y);
    }

    // KF-334: the card's column comes from its list; cards carry no column id.
    function cardColumnId(card) {
      var list = card ? card.closest('.task-list') : null;
      return list ? list.dataset.columnId : null;
    }


    function hideCardSubmenu() {
      var sub = document.getElementById('card-ctx-submenu');
      if (sub) { sub.hidden = true; sub.innerHTML = ''; sub.classList.remove('card-sub--color', 'card-sub--epics'); sub.dataset.for = ''; }
    }

    // KF-334: the submenu opens beside the card menu, which stays open
    // (KanbanFlow shows both side by side).
    function openCardSubmenu(btn, buildItems) {
      var sub = document.getElementById('card-ctx-submenu');
      sub.innerHTML = '';
      sub.classList.remove('card-sub--color', 'card-sub--epics');
      sub.dataset.for = '';
      buildItems(sub);
      var r = btn.getBoundingClientRect();
      positionMenu(sub, r.right, r.top);
      // Flip to the left when the submenu would run off the viewport.
      var sr = sub.getBoundingClientRect();
      if (r.right + sr.width > window.innerWidth - 4) {
        positionMenu(sub, r.left - sr.width, r.top);
      }
    }

    // KF-334: Move submenu icons (16px line icons like the card menu's).
    var CARD_SUB_ICONS = {
      'move-right': '<path d="M2.5 8h11M9.5 4l4 4-4 4"/>',
      'move-top': '<path d="M8 14V5M4.5 8.5L8 5l3.5 3.5M3 2h10"/>',
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

    var epicSubmenuLoading = false;
    var epicHoverTried = false;
    var hoveredAct = null;

    function openEpicSubmenu(btn, task) {
      var card = cardMenuCard;
      if (!card || epicSubmenuLoading) return;
      epicSubmenuLoading = true;
      BoardChrome.epicChoices(task.id).then(function (choices) {
        epicSubmenuLoading = false;
        if (cardMenuCard !== card || document.getElementById('card-ctx-menu').hidden) return;
        if (hoveredAct !== null && hoveredAct !== 'add-to-epic') return;
        if (!choices) { toast('Could not load epics.'); return; }
        openCardSubmenu(btn, function (sub) {
          sub.dataset.for = CARD_SUBMENU.EPIC;
          sub.classList.add('card-sub--epics');
          appendEpicChoices(sub, choices, function (label, epicId) {
            return cardSubmenuButton(label, CARD_SUBMENU.EPIC, { 'data-epic-id': epicId });
          });
        });
      }, function () { epicSubmenuLoading = false; });
    }

    function hoverCardMenuItem(item) {
      var sub = document.getElementById('card-ctx-submenu');
      var act = item.getAttribute('data-card-act');
      hoveredAct = act;
      if (act === CARD_SUBMENU.MOVE) {
        if (sub.hidden || sub.dataset.for !== CARD_SUBMENU.MOVE) item.click();
      } else if (act === 'add-to-epic') {
        if (epicSubmenuLoading || (!sub.hidden && sub.dataset.for === CARD_SUBMENU.EPIC)) return;
        if (BoardChrome.epicsLoaded || !epicHoverTried) {
          epicHoverTried = true;
          item.click();
        }
      } else if (sub.dataset.for === CARD_SUBMENU.MOVE || sub.dataset.for === CARD_SUBMENU.EPIC) {
        hideCardSubmenu();
      }
    }

    document.getElementById('card-ctx-menu').addEventListener('mouseover', function (e) {
      var item = e.target.closest('[data-card-act]');
      if (item) hoverCardMenuItem(item);
    });

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
      } else if (act === CARD_SUBMENU.MOVE) {
        openCardSubmenu(btn, function (sub) {
          // KF-334: KanbanFlow-style relative-move submenu.
          var card = cardMenuCard;
          if (!card) return;
          var currentColumnId = cardColumnId(card);
          var headers = Array.prototype.slice.call(document.querySelectorAll('#column-headers-row .columnHeader'));
          var currentIndex = headers.findIndex(function (h) { return h.dataset.columnId === currentColumnId; });
          sub.dataset.for = CARD_SUBMENU.MOVE;
          if (!card.closest('[data-done-column="true"]')) {
            // "Move right" only when a column exists to the right.
            if (currentIndex >= 0 && currentIndex < headers.length - 1) {
              var nextColumnId = headers[currentIndex + 1].dataset.columnId;
              sub.appendChild(cardSubmenuButton('Move right', 'move-right', { 'data-column-id': nextColumnId }));
            }
            sub.appendChild(cardSubmenuButton('Move to top', 'move-top'));
            sub.appendChild(cardSubmenuButton('Move to bottom', 'move-bottom'));
          }
          // "Move advanced" (always available).
          sub.appendChild(cardSubmenuButton('Move advanced', 'move-advanced'));
        });
      } else if (act === 'add-to-epic') {
        openEpicSubmenu(btn, task);
      } else if (act === CARD_SUBMENU.COLOR) {
        openCardSubmenu(btn, function (sub) {
          // KF-333: each row is tinted with its colour; the card's own colour
          // is bold.
          sub.classList.add('card-sub--color');
          var current = cardMenuCard ? cardMenuCard.dataset.colorValue : '';
          var src = document.getElementById('board-colors');
          if (src) {
            Array.prototype.forEach.call(src.querySelectorAll('span[data-id]'), function (s) {
              var b = cardSubmenuButton(s.dataset.label || s.dataset.value, CARD_SUBMENU.COLOR,
                { 'data-color-id': s.dataset.id, 'data-color-value': s.dataset.value,
                  // KF-142: carry the human label so the card's title can be re-synced.
                  'data-color-label': s.dataset.label || '' });
              b.style.backgroundColor = s.dataset.bg || '#fff';
              if (s.dataset.value === current) b.classList.add('card-color-current');
              sub.appendChild(b);
            });
          }
        });
      } else {
        hideCardSubmenu();
        hideFloatingMenus();
        if (act === 'assign-members') openMembersDialog(task ? task.id : null);
        else if (act === 'grouping-date') openGroupingDateDialog(cardMenuCard, cardMenuPoint);
        else if (act === 'copy-here') copyCardHere(task);
        else if (act === 'copy') openCopyDialog(task ? task.id : null);
        else if (act === 'task-url') copyTaskUrl(task ? task.id : null);
        else if (act === 'delete') deleteCardTask(task);
        else if (act === 'make-epic') makeCardEpic(task);
      }
    });

    var bulkCards = [];

    function bulkIds() {
      return bulkCards.map(function (card) { return card.dataset.taskId; });
    }

    function openBulkMenu(x, y) {
      bulkCards = selectedCards();
      var anyMember = bulkCards.some(function (card) { return !!card.dataset.memberIds; });
      var menu = document.getElementById('bulk-ctx-menu');
      menu.querySelector('[data-bulk-act="members"]').hidden = anyMember;
      menu.querySelector('[data-bulk-act="members-menu"]').hidden = !anyMember;
      placeMenu(menu, x, y);
    }

    function bulkSubmenuButton(label, kind, extra) {
      var b = document.createElement('button');
      b.type = 'button';
      b.textContent = label;
      b.setAttribute('data-bulk-sub', kind);
      Object.keys(extra || {}).forEach(function (k) { b.setAttribute(k, extra[k]); });
      return b;
    }

    document.getElementById('bulk-ctx-menu').addEventListener('click', function (e) {
      var btn = e.target.closest('[data-bulk-act]');
      if (!btn || !bulkCards.length) return;
      var act = btn.getAttribute('data-bulk-act');
      if (act === BULK_ACT.COLOR) {
        openCardSubmenu(btn, function (sub) {
          sub.classList.add('card-sub--color');
          var src = document.getElementById('board-colors');
          if (!src) return;
          Array.prototype.forEach.call(src.querySelectorAll('span[data-id]'), function (s) {
            var b = bulkSubmenuButton(s.dataset.label || s.dataset.value, BULK_SUB.COLOR, { 'data-color-id': s.dataset.id });
            b.style.backgroundColor = s.dataset.bg || '#fff';
            sub.appendChild(b);
          });
        });
      } else if (act === BULK_ACT.MEMBERS_MENU) {
        openCardSubmenu(btn, function (sub) {
          sub.appendChild(bulkSubmenuButton('Assign members', BULK_SUB.MEMBERS));
          sub.appendChild(bulkSubmenuButton('Unassign all members', BULK_SUB.UNASSIGN));
        });
      } else {
        var ids = bulkIds();
        var anchor = bulkCards[0];
        hideCardSubmenu();
        hideFloatingMenus();
        if (act === BULK_ACT.MOVE) openMoveDialog(null, anchor, ids);
        else if (act === BULK_ACT.MEMBERS) openBulkMembersDialog(ids);
        else if (act === BULK_ACT.DELETE) deleteSelection();
      }
    });

    document.getElementById('card-ctx-submenu').addEventListener('click', function (e) {
      var btn = e.target.closest('[data-bulk-sub]');
      if (!btn || !bulkCards.length) return;
      var kind = btn.getAttribute('data-bulk-sub');
      var ids = bulkIds();
      var colorId = btn.getAttribute('data-color-id');
      hideCardSubmenu();
      hideFloatingMenus();
      if (kind === BULK_SUB.COLOR) bulkColor(ids, colorId);
      else if (kind === BULK_SUB.MEMBERS) openBulkMembersDialog(ids);
      else if (kind === BULK_SUB.UNASSIGN) bulkUnassign(ids);
    });

    function openGroupingDateDialog(card, point) {
      var overlay = document.getElementById('grouping-date-dialog');
      var dlg = overlay.querySelector('.dlg');
      groupingDateTaskId = card.dataset.taskId;
      document.getElementById('gd-input').value = card.dataset.groupDay || '';
      overlay.hidden = false;
      dlg.style.left = Math.max(0, Math.min(point.x, window.innerWidth - dlg.offsetWidth)) + 'px';
      dlg.style.top = Math.max(0, Math.min(point.y, window.innerHeight - dlg.offsetHeight)) + 'px';
      document.getElementById('gd-input').focus();
    }

    document.getElementById('gd-update').addEventListener('click', function () {
      var date = document.getElementById('gd-input').value;
      if (!groupingDateTaskId || !date) { toast('Pick a date.'); return; }
      api('/api/tasks/' + encodeURIComponent(groupingDateTaskId), 'PATCH', { grouping_date: date })
        .then(function (res) {
          if (!res.ok) { toast('Could not save the grouping date.'); return; }
          document.getElementById('grouping-date-dialog').hidden = true;
          return drained(res).then(function () { window.location.reload(); });
        });
    });

    document.getElementById('grouping-date-dialog').addEventListener('click', function (e) {
      if (e.target === e.currentTarget) e.currentTarget.hidden = true;
    });

    document.getElementById('card-ctx-submenu').addEventListener('click', function (e) {
      var btn = e.target.closest('[data-card-sub]');
      if (!btn || !cardMenuCard) return;
      var sub = btn.getAttribute('data-card-sub');
      var task = cardMenuTask();
      hideCardSubmenu();
      hideFloatingMenus();
      if (sub === CARD_SUBMENU.EPIC) {
        addCardToEpic(cardMenuCard, btn.getAttribute('data-epic-id'));
      } else if (sub === 'move-right') {
        // KF-334: move to the column to the right, placed per the board's setting.
        var colId = btn.getAttribute('data-column-id');
        var curList = cardMenuCard ? cardMenuCard.closest('.task-list') : null;
        var curLane = curList ? (curList.dataset.swimlaneId || null) : null;
        var listSel = '.task-list[data-column-id="' + cssEscape(colId) + '"]';
        var targetList = (curLane && document.querySelector(listSel + '[data-swimlane-id="' + cssEscape(curLane) + '"]')) ||
            document.querySelector(listSel);
        var laneId = targetList ? (targetList.dataset.swimlaneId || null) : curLane;
        api('/api/tasks/' + encodeURIComponent(task.id) + '/move?force=true', 'POST',
            { column_id: colId, swimlane_id: laneId })
          .then(drained)
          .then(function (res) {
            if (res.ok) window.location.reload();
            else toastRefusal(res, 'Could not move task.');
          });
      } else if (sub === 'move-bottom' || sub === 'move-top') {
        // KF-334: move to the top or bottom of the current column.
        var curList = cardMenuCard ? cardMenuCard.closest('.task-list') : null;
        var curLane = curList ? (curList.dataset.swimlaneId || null) : null;
        var colId = cardColumnId(cardMenuCard);
        var listSel = '.task-list[data-column-id="' + cssEscape(colId) + '"]';
        var targetList = (curLane && document.querySelector(listSel + '[data-swimlane-id="' + cssEscape(curLane) + '"]')) ||
            document.querySelector(listSel);
        var laneId = targetList ? (targetList.dataset.swimlaneId || null) : curLane;
        var position = targetList && sub === 'move-bottom' ? targetList.querySelectorAll('.task-card').length : 0;
        api('/api/tasks/' + encodeURIComponent(task.id) + '/move?force=true', 'POST',
            { column_id: colId, swimlane_id: laneId, position: position })
          .then(drained)
          .then(function (res) {
            if (res.ok) window.location.reload();
            else toastRefusal(res, 'Could not move task.');
          });
      } else if (sub === 'move-advanced') {
        // KF-334/KF-141: the "Move task" popover, anchored at the card.
        openMoveDialog(task.id, cardMenuCard);
      } else if (sub === CARD_SUBMENU.COLOR) {
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
      }).then(drained).then(function (res) {
        if (res.ok) window.location.reload();
        else toastRefusal(res, 'Could not copy task.');
      });
    }

    function deleteCardTask(task) {
      if (!task) return;
      // KF-184: styled in-page confirmation (KanbanFlow parity); the native
      // window.confirm() auto-dismisses under automation, silently no-op'ing.
      showConfirmDialog('Delete task', deleteConfirmMessage(task.name, cardMenuCard), 'Delete', function () {
        api('/api/tasks/' + encodeURIComponent(task.id), 'DELETE')
          .then(drained)
          .then(function (res) {
            if (!res.ok) { toast('Could not delete task.'); return; }
            // KF-193: the header count went stale after context-menu deletes —
            // capture the column before detaching the card, then refresh it
            // (mirrors the add-task and drag/drop paths via updateColumnCount).
            var list = cardMenuCard ? cardMenuCard.closest('.task-list') : null;
            var colId = list ? list.dataset.columnId : null;
            var estimate = cardEstimateMinutes(cardMenuCard);
            const wasEpic = !!cardMenuCard && cardMenuCard.dataset.kind === CARD_KIND.EPIC;
            if (cardMenuCard && cardMenuCard.parentNode) cardMenuCard.remove();
            cardMenuCard = null;
            if (window.BoardChrome && BoardChrome.refold) BoardChrome.refold();
            if (wasEpic) BoardChrome.refreshMarks();
            else if (colId) updateColumnCount(colId, -1, -estimate);
            syncTimerAfterDelete();
          });
      });
    }

    // KF-501: deleting the timed task idles the timer; the open board
    // follows now instead of at the next poll.
    function syncTimerAfterDelete() {
      if (!window.TimerUI) return;
      window.TimerUI.refresh();
      window.TimerUI.broadcastTimer();
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
    var acNameInput = document.getElementById('ac-name');
    acNameInput.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') { e.preventDefault(); doAddColumn(); }
    });
    ['add-column-dialog', 'swimlane-dialog'].forEach(function (overlayId) {
      document.getElementById(overlayId).addEventListener('click', function (e) {
        if (e.target === e.currentTarget) e.currentTarget.hidden = true;
      });
    });
    document.getElementById('le-save').addEventListener('click', doSaveLane);
    document.getElementById('le-name').addEventListener('keydown', onLaneNameKeydown);
    document.getElementById('le-add-limit').addEventListener('click', function () { openLimitPopup(-1); });
    document.getElementById('ll-save').addEventListener('click', doSaveLimit);
    document.getElementById('ll-close').addEventListener('click', function () {
      document.getElementById('lane-limit-popup').hidden = true;
    });
    initLaneResize();
    document.getElementById('ec-save').addEventListener('click', doSaveColumn);
    initTaskDialogs();
    // KF-367: the layout view's "Save board as template" opens the dialog.
    var saveTplBtn = document.getElementById('save-template-btn');
    if (saveTplBtn) saveTplBtn.addEventListener('click', openSaveTemplateDialog);
    document.getElementById('st-save').addEventListener('click', doSaveTemplate);
  }

  function showPendingToast() {
    var message = null;
    try {
      message = window.sessionStorage.getItem('pomb.pendingToast');
      window.sessionStorage.removeItem('pomb.pendingToast');
    } catch (e) {}
    if (message) toast(message);
  }

  var taskDialogsInitialized = false;
  function initTaskDialogs() {
    if (taskDialogsInitialized) return;
    taskDialogsInitialized = true;
    document.getElementById('mt-move-btn').addEventListener('click', doMoveTask);
    document.getElementById('cp-copy-btn').addEventListener('click', doCopyTask);
    document.getElementById('cp-name').addEventListener('keydown', function (e) {
      if (e.key === 'Enter') { e.preventDefault(); doCopyTask(); }
    });
    // KF-334: a click outside the card-anchored popover closes it.
    document.getElementById('move-task-dialog').addEventListener('click', function (e) {
      if (e.target === e.currentTarget && e.currentTarget.classList.contains('dlg-overlay--popover')) {
        e.currentTarget.hidden = true;
      }
    });
    document.getElementById('est-add').addEventListener('click', doAddEstimate);
    initRelationDialog();
    // KF-547: a click outside the estimate popover closes it.
    document.getElementById('estimate-dialog').addEventListener('click', function (e) {
      if (e.target === e.currentTarget) e.currentTarget.hidden = true;
    });
    // KF-505: a click outside the due popover closes it.
    document.getElementById('duedate-dialog').addEventListener('click', function (e) {
      if (e.target === e.currentTarget) e.currentTarget.hidden = true;
    });
  }

  function initReportModal() {
    if (document.querySelector('.bshell-report') && document.getElementById('move-task-dialog')) initTaskDialogs();
  }

  // ---------- columns ----------

  function moveColumnBy(id, dir) {
    var headers = Array.prototype.slice.call(document.querySelectorAll('#column-headers-row .columnHeader'));
    var index = headers.findIndex(function (h) { return h.dataset.columnId === id; });
    if (index < 0) return;
    var target = index + dir;
    if (target < 0 || target >= headers.length) return;
    api('/api/columns/' + encodeURIComponent(id) + '/move', 'POST', { position: target })
      .then(drained)
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

  function deleteConfirmMessage(name, card) {
    var list = card && card.closest ? card.closest('.task-list') : null;
    var header = list ? document.querySelector(
      '.columnHeader[data-column-id="' + cssEscape(list.dataset.columnId) + '"]') : null;
    var message = 'Delete "' + name + '"?';
    if (header && header.dataset.isDone === '1') {
      message += ' Deleted tasks are removed permanently.';
    }
    return message;
  }

  // KF-417: KanbanFlow-style alert box with a single OK.
  function showAlertDialog(message) {
    document.getElementById('alert-message').textContent = message;
    document.getElementById('alert-dialog').hidden = false;
    document.getElementById('alert-ok').focus();
  }

  // KF-432: KanbanFlow's confirm before a past due date is used. Cancel
  // has focus, as in golden modal/26, and leaves the due date dialog open.
  function showPastDueConfirm(onOk) {
    var dlg = document.getElementById('past-due-dialog');
    var okBtn = document.getElementById('past-due-ok');
    var fresh = okBtn.cloneNode(true);
    okBtn.parentNode.replaceChild(fresh, okBtn);
    fresh.addEventListener('click', function () {
      dlg.hidden = true;
      onOk();
    });
    dlg.hidden = false;
    document.getElementById('past-due-cancel').focus();
  }
  // KF-371: "Load more tasks" in an archiving column shows its next page
  // of 20 tasks and the date labels that now have a visible task.
  document.addEventListener('click', function (e) {
    var more = e.target.closest ? e.target.closest('[data-load-more]') : null;
    if (!more) return;
    const list = more.closest('.task-list');
    if (window.BoardChrome && BoardChrome.pageArchive) BoardChrome.pageArchive(list, true);
    if (window.BoardChrome && BoardChrome.refold) BoardChrome.refold();
  });

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
        showError('Could not delete board: ' + section.dataset.lastBoard);
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

  // KF-554: General tab. Edit opens the rename form; Archive board runs the
  // delete page's archive call (its 400 refusals, KF-407/KF-492, show inline);
  // Delete board opens the delete page, which owns the delete refusals.
  var boardGeneralInitialized = false;
  function initBoardGeneralActions() {
    var section = document.querySelector('.kf-general');
    if (!section || boardGeneralInitialized) return;
    boardGeneralInitialized = true;
    var id = section.dataset.boardId;
    var err = document.getElementById('board-action-error');
    document.getElementById('board-name-edit').addEventListener('click', function () {
      document.getElementById('board-name-view').hidden = true;
      var form = document.getElementById('board-name-form');
      form.hidden = false;
      form.elements.name.focus();
    });
    document.getElementById('board-name-form').addEventListener('submit', (ev) => {
      ev.preventDefault();
      err.hidden = true;
      api(`/api/boards/${encodeURIComponent(id)}/config`, 'PUT', { name: ev.target.elements.name.value })
        .then((res) => {
          if (res.ok) return res;
          return res.text().then((t) => { throw new Error(t || `HTTP ${res.status}`); });
        })
        .then(() => { window.location.href = `/b/${encodeURIComponent(id)}/settings?saved=1`; })
        .catch((ex) => {
          err.textContent = ex.message;
          err.hidden = false;
        });
    });
    var arcBtn = document.getElementById('board-archive-btn');
    arcBtn.addEventListener('click', function () {
      arcBtn.disabled = true;
      err.hidden = true;
      api('/api/boards/' + encodeURIComponent(id) + '/config', 'PUT', { archived: true })
        .then(function (res) {
          if (res.ok) return res;
          return res.text().then(function (t) { throw new Error(t || ('HTTP ' + res.status)); });
        })
        .then(function () { window.location.href = '/boards/archived'; })
        .catch(function (ex) {
          arcBtn.disabled = false;
          err.textContent = 'Could not archive board: ' + ex.message;
          err.hidden = false;
        });
    });
    var delBtn = document.getElementById('board-delete-btn');
    delBtn.addEventListener('click', function () { window.location.href = delBtn.dataset.href; });
    initChangeBoardOwner(id, err);
  }

  function initChangeBoardOwner(boardId, err) {
    var view = document.getElementById('board-owner-view');
    var form = document.getElementById('board-owner-form');
    var select = document.getElementById('board-owner-select');
    var url = '/api/boards/' + encodeURIComponent(boardId);
    var show = function (editing) {
      view.hidden = editing;
      form.hidden = !editing;
    };
    var fail = function (message) {
      err.textContent = message;
      err.hidden = false;
    };
    var failOnError = function (res) {
      if (res.ok) return res;
      return res.text().then(function (t) { throw new Error(t || ('HTTP ' + res.status)); });
    };
    document.getElementById('board-owner-change').addEventListener('click', function () {
      err.hidden = true;
      api(url + '/members', 'GET')
        .then(failOnError)
        .then(function (res) { return res.json(); })
        .then(function (members) {
          var others = members.filter(function (m) { return m.role !== 'owner'; });
          if (!others.length) {
            fail('Add a member to the board first, then hand it over.');
            return;
          }
          select.innerHTML = others.map(function (m) {
            return '<option value="' + escapeHtml(m.username) + '">' + escapeHtml(m.username) + '</option>';
          }).join('');
          show(true);
          select.focus();
        })
        .catch(function (ex) { fail('Could not load the members: ' + ex.message); });
    });
    document.getElementById('board-owner-cancel').addEventListener('click', function () { show(false); });
    form.addEventListener('submit', function (e) {
      e.preventDefault();
      err.hidden = true;
      api(url + '/owner', 'PUT', { username: select.value })
        .then(failOnError)
        .then(function () { window.location.href = '/b/' + encodeURIComponent(boardId); })
        .catch(function (ex) { fail('Could not change the owner: ' + ex.message); });
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

    // KF-479: a board keeps at least one column.
    if (!targets.length) {
      toast('A board needs at least one column.');
      return;
    }

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
            // Drain the body first so the reload does not abort it.
            return res.text().then(function () {
              if (res.ok) window.location.reload();
              else toast('Could not delete column.');
            });
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
      cleanup();

      // KF-405: each task keeps its lane and joins the end of the target
      // column's cell there, in its current order.
      var nextPosition = {};
      var chain = Promise.resolve();
      Array.prototype.forEach.call(cards, function (card) {
        var lane = card.closest('.task-list').dataset.swimlaneId || '';
        if (!(lane in nextPosition)) {
          nextPosition[lane] = document.querySelectorAll('.task-list[data-column-id="' + cssEscape(targetId) +
            '"][data-swimlane-id="' + cssEscape(lane) + '"] .task-card').length;
        }
        var body = { column_id: targetId, position: nextPosition[lane]++ };
        if (lane) body.swimlane_id = lane;
        chain = chain.then(function () {
          return api('/api/tasks/' + encodeURIComponent(card.dataset.taskId) + '/move?force=true', 'POST', body)
            .then(function (res) { if (!res.ok) throw new Error('move failed'); });
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

  function openAddColumnDialog(placement, anchor) {
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
      var posSelect = document.getElementById('ac-position');
      posSelect.selectedIndex = placement === 'beginning' ? 0 : posSelect.options.length - 1;
    }
    showPopover('add-column-dialog', anchor);
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
      if (typeof placement === 'string' && placement.indexOf('after:') === 0) {
        placement = { anchor: placement.slice('after:'.length), side: 'right' };
      }
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
      .then(function (res) { return res ? drained(res) : res; })
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
    document.getElementById('ec-wip-type').value = th.dataset.wipUnit === 'hours' ? 'hours' : 'count';
    document.getElementById('ec-collapsed').checked = th.dataset.collapsed === '1';
    document.getElementById('ec-blocked').checked = th.dataset.isBlocked === '1';
    document.getElementById('ec-stage').value = th.dataset.stage || '';
    // KF-042: the remaining fields live in the column's opaque config_json
    // bag — load the saved values so editing a name doesn't wipe them.
    var cfg = columnConfigBag(th);
    document.getElementById('ec-sorting').value = cfg.sorting || 'none';
    // KF-045: column sum is a dropdown ("None"/"Show sum"), not a checkbox.
    document.getElementById('ec-column-sum').value =
      cfg.column_sum === true ? 'count' : (cfg.column_sum === 'count' || cfg.column_sum === 'hours' ? cfg.column_sum : 'none');
    // KF-384: Archiving shows the saved value, else on for the Done column
    // (the server applies the same default, KF-371).
    document.getElementById('ec-group-by-date').checked =
      typeof cfg.group_by_date === 'boolean' ? cfg.group_by_date : th.dataset.isDone === '1';
    var props = columnPropConfig(th);
    if (props.due_dates === 'hide') props.due_dates = 'active_7d';
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
      wip_limit_type: document.getElementById('ec-wip-type').value,
      sorting: document.getElementById('ec-sorting').value,
      // KF-371: Column sum is stored as "none", "count" or "hours".
      column_sum: document.getElementById('ec-column-sum').value,
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
      is_blocked: document.getElementById('ec-blocked').checked,
      stage: document.getElementById('ec-stage').value || null,
      config_json: JSON.stringify(config),
    }).then(drained).then(function (res) {
      if (res.ok) window.location.reload();
      else toast('Could not save column.');
    });
  }

  // ---------- swimlanes ----------

  function laneOrder() {
    return Array.prototype.map.call(document.querySelectorAll('#lane-order [data-lane-id]'), function (el) {
      var limits = [];
      try { limits = JSON.parse(el.dataset.laneLimits || '[]'); } catch (err) { limits = []; }
      return {
        id: el.dataset.laneId,
        name: el.dataset.laneName,
        description: el.dataset.laneDescription || '',
        limits: limits,
        collapsed: el.dataset.laneCollapsed === '1',
        count: parseInt(el.dataset.taskCount, 10) || 0,
      };
    });
  }

  function laneById(id) {
    return laneOrder().find(function (l) { return l.id === id; }) || null;
  }

  function boardColumns() {
    return Array.prototype.map.call(document.querySelectorAll('#board-column-options [data-column-id]'), function (el) {
      return { id: el.dataset.columnId, name: el.dataset.columnName };
    });
  }

  function reloadAfter(request, failure) {
    request.then(drained).then(function (res) {
      if (res.ok) window.location.reload(); else toast(failure);
    }).catch(function () { toast(failure); });
  }

  function patchLane(id, body, failure) {
    reloadAfter(api('/api/swimlanes/' + encodeURIComponent(id), 'PATCH', body), failure);
  }

  function createLaneAt(name, position) {
    reloadAfter(api('/api/swimlanes', 'POST', { board_id: boardId(), name: name, position: position }),
      'Could not add swimlane.');
  }

  function toggleLaneFold(id) {
    var lane = laneById(id);
    if (!lane) return;
    patchLane(id, { collapsed: !lane.collapsed }, 'Could not update the swimlane.');
  }

  function moveSwimlane(id, dir) {
    var lanes = laneOrder();
    var index = lanes.findIndex(function (l) { return l.id === id; });
    if (index < 0) return;
    var target = index + dir;
    if (target < 0 || target >= lanes.length) return;
    reloadAfter(api('/api/swimlanes/' + encodeURIComponent(id) + '/move', 'POST', { position: target }),
      'Could not move swimlane.');
  }

  function confirmMergeAll() {
    showConfirmDialog('Merge all swimlanes?',
      'This action can NOT be undone. All tasks will be merged into a single lane without order guaranteed.',
      'Yes, merge them', function () {
        reloadAfter(api('/api/boards/' + encodeURIComponent(boardId()) + '/swimlanes/merge', 'POST'),
          'Could not merge the swimlanes.');
      });
  }

  function deleteSwimlane(id) {
    var lanes = laneOrder();
    var lane = lanes.find(function (l) { return l.id === id; });
    if (!lane) return;
    if (lanes.length < 2) {
      toast('A board needs at least one swimlane.');
      return;
    }
    if (lane.count > 0) {
      confirmMergeAll();
      return;
    }
    showConfirmDialog('Delete swimlane',
      'Are you sure you want to delete the swimlane ' + lane.name + '?', 'Delete', function () {
        reloadAfter(api('/api/swimlanes/' + encodeURIComponent(id), 'DELETE'), 'Could not delete swimlane.');
      });
  }


  function inlineLaneForm(options) {
    var li = document.createElement('li');
    li.className = 'layout-list-item layout-list-inline';
    var input = document.createElement('input');
    input.type = 'text';
    input.maxLength = 200;
    input.autocomplete = 'off';
    input.value = options.value || '';
    input.setAttribute('aria-label', options.label);
    var cancel = document.createElement('button');
    cancel.type = 'button';
    cancel.className = 'btn';
    cancel.textContent = 'Cancel';
    var ok = document.createElement('button');
    ok.type = 'button';
    ok.className = 'btn btn-success';
    ok.textContent = options.primary;
    li.appendChild(input);
    li.appendChild(cancel);
    li.appendChild(ok);
    function submit() {
      var name = input.value.trim();
      if (!name) { toast('Swimlane name is required.'); input.focus(); return; }
      options.onSubmit(name, li);
    }
    cancel.addEventListener('click', options.onCancel);
    ok.addEventListener('click', submit);
    input.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') { e.preventDefault(); submit(); }
      else if (e.key === 'Escape') { e.preventDefault(); options.onCancel(); }
    });
    return li;
  }

  function clearInlineLaneForms() {
    document.querySelectorAll('#layout-swimlanes-list .layout-list-inline').forEach(function (li) { li.remove(); });
    document.querySelectorAll('#layout-swimlanes-list [data-swimlane-id][hidden]').forEach(function (li) { li.hidden = false; });
  }

  function promptFirstLaneNames(at) {
    var list = document.getElementById('layout-swimlanes-list');
    var lane = laneOrder()[0];
    var item = list && list.querySelector('[data-swimlane-id]');
    if (!lane || !item) return;
    clearInlineLaneForms();
    item.hidden = true;
    function cancel() { clearInlineLaneForms(); }
    var first = inlineLaneForm({
      label: 'Name of the existing swimlane',
      value: lane.name,
      primary: 'Next',
      onCancel: cancel,
      onSubmit: function (existingName, firstForm) {
        firstForm.classList.add('layout-list-inline--done');
        firstForm.querySelector('input').readOnly = true;
        firstForm.querySelectorAll('button').forEach(function (b) { b.hidden = true; });
        var second = inlineLaneForm({
          label: 'Name of the new swimlane',
          primary: 'Add',
          onCancel: cancel,
          onSubmit: function (newName) {
            api('/api/swimlanes/' + encodeURIComponent(lane.id), 'PATCH', { name: existingName })
              .then(drained)
              .then(function (res) {
                if (!res.ok) { toast('Could not rename the swimlane.'); return null; }
                return api('/api/swimlanes', 'POST', { board_id: boardId(), name: newName, position: at });
              })
              .then(function (res) { return res ? drained(res) : res; })
              .then(function (res) {
                if (res && res.ok) window.location.reload();
                else if (res) toast('Could not add swimlane.');
              })
              .catch(function () { toast('Could not add swimlane.'); });
          },
        });
        list.insertBefore(second, at === 0 ? firstForm : firstForm.nextSibling);
        second.querySelector('input').focus();
      },
    });
    list.insertBefore(first, item);
    first.querySelector('input').focus();
  }

  function addLaneBeside(id, side) {
    var lanes = laneOrder();
    var index = lanes.findIndex(function (l) { return l.id === id; });
    if (index < 0) return;
    var at = side === 'above' ? index : index + 1;
    if (lanes.length === 1) { promptFirstLaneNames(at); return; }
    var item = document.querySelector('#layout-swimlanes-list [data-swimlane-id="' + cssEscape(id) + '"]');
    if (!item) return;
    clearInlineLaneForms();
    var form = inlineLaneForm({
      label: 'Swimlane name',
      primary: 'Add',
      onCancel: clearInlineLaneForms,
      onSubmit: function (name) { createLaneAt(name, at); },
    });
    item.parentNode.insertBefore(form, side === 'above' ? item : item.nextSibling);
    form.querySelector('input').focus();
  }

  function addSwimlane(anchor) {
    var lanes = laneOrder();
    if (lanes.length === 1) { promptFirstLaneNames(1); return; }
    var select = document.getElementById('sw-position');
    select.textContent = '';
    var first = document.createElement('option');
    first.value = '0';
    first.textContent = 'Before "' + lanes[0].name + '"';
    select.appendChild(first);
    lanes.forEach(function (lane, index) {
      var option = document.createElement('option');
      option.value = String(index + 1);
      option.textContent = 'After "' + lane.name + '"';
      select.appendChild(option);
    });
    select.selectedIndex = select.options.length - 1;
    document.getElementById('sw-name').value = '';
    showPopover('swimlane-dialog', anchor);
    document.getElementById('sw-name').focus();
  }

  function doSaveSwimlane() {
    var name = document.getElementById('sw-name').value.trim();
    if (!name) { toast('Swimlane name is required.'); return; }
    document.getElementById('swimlane-dialog').hidden = true;
    createLaneAt(name, parseInt(document.getElementById('sw-position').value, 10));
  }

  function showPopover(id, anchor) {
    var overlay = document.getElementById(id);
    var dlg = overlay.querySelector('.dlg');
    dlg.style.left = '';
    dlg.style.top = '';
    overlay.hidden = false;
    var w = dlg.offsetWidth;
    var h = dlg.offsetHeight;
    var r = anchor ? anchor.getBoundingClientRect() : null;
    var left = r ? r.left : (window.innerWidth - w) / 2;
    var top = r ? r.bottom + 4 : 80;
    dlg.style.left = Math.max(8, Math.min(left, window.innerWidth - w - 8)) + 'px';
    dlg.style.top = Math.max(8, Math.min(top, window.innerHeight - h - 8)) + 'px';
  }


  var editLaneId = null;
  var editLaneLimits = [];
  var editLaneDescription = '';
  var limitEditIndex = -1;

  function columnName(id) {
    var col = boardColumns().find(function (c) { return c.id === id; });
    return col ? col.name : '';
  }

  function openEditLaneDialog(id) {
    var lane = laneById(id);
    if (!lane) return;
    editLaneId = id;
    editLaneLimits = lane.limits.map(function (l) { return { column_id: l.column_id, max: l.max }; });
    document.getElementById('le-name').value = lane.name;
    editLaneDescription = lane.description.replace(/\r\n?/g, '\n');
    document.getElementById('le-description').value = lane.description;
    document.getElementById('lane-limit-popup').hidden = true;
    renderLaneLimits();
    document.getElementById('lane-edit-dialog').hidden = false;
    document.getElementById('le-name').focus();
  }

  function renderLaneLimits() {
    var list = document.getElementById('le-limits');
    list.textContent = '';
    editLaneLimits.forEach(function (limit, index) {
      var li = document.createElement('li');
      li.className = 'lane-limit-item';
      var label = document.createElement('span');
      var strong = document.createElement('strong');
      strong.textContent = columnName(limit.column_id) + ':';
      label.appendChild(strong);
      label.appendChild(document.createTextNode(' Max ' + limit.max + (limit.max === 1 ? ' task' : ' tasks')));
      var kebab = document.createElement('button');
      kebab.type = 'button';
      kebab.className = 'lane-limit-kebab';
      kebab.setAttribute('aria-label', 'Limit options');
      kebab.innerHTML = '<svg class="ico-kebab" width="4" height="16" viewBox="0 0 4 16" fill="currentColor" aria-hidden="true" focusable="false"><circle cx="2" cy="2.5" r="1.5"/><circle cx="2" cy="8" r="1.5"/><circle cx="2" cy="13.5" r="1.5"/></svg>';
      var menu = document.createElement('div');
      menu.className = 'lane-limit-menu';
      menu.hidden = true;
      var edit = document.createElement('button');
      edit.type = 'button';
      edit.textContent = 'Edit';
      var del = document.createElement('button');
      del.type = 'button';
      del.textContent = 'Delete';
      menu.appendChild(edit);
      menu.appendChild(del);
      kebab.addEventListener('click', function () {
        var open = menu.hidden;
        list.querySelectorAll('.lane-limit-menu').forEach(function (m) { m.hidden = true; });
        menu.hidden = !open;
      });
      edit.addEventListener('click', function () { menu.hidden = true; openLimitPopup(index); });
      del.addEventListener('click', function () {
        editLaneLimits.splice(index, 1);
        renderLaneLimits();
      });
      li.appendChild(label);
      li.appendChild(kebab);
      li.appendChild(menu);
      list.appendChild(li);
    });
  }

  function openLimitPopup(index) {
    limitEditIndex = index;
    var select = document.getElementById('ll-column');
    select.textContent = '';
    var taken = editLaneLimits.map(function (l) { return l.column_id; });
    var editing = index >= 0 ? editLaneLimits[index] : null;
    boardColumns().forEach(function (col) {
      if (taken.indexOf(col.id) >= 0 && !(editing && editing.column_id === col.id)) return;
      var option = document.createElement('option');
      option.value = col.id;
      option.textContent = col.name;
      select.appendChild(option);
    });
    if (!select.options.length) { toast('Every column already has a limit.'); return; }
    if (editing) select.value = editing.column_id;
    document.getElementById('ll-max').value = editing ? String(editing.max) : '3';
    document.getElementById('ll-title').textContent = editing ? 'Edit limit' : 'Add limit';
    document.getElementById('ll-save').textContent = editing ? 'Update limit' : 'Add limit';
    document.getElementById('lane-limit-popup').hidden = false;
  }

  function doSaveLimit() {
    var column = document.getElementById('ll-column').value;
    var max = parseInt(document.getElementById('ll-max').value, 10);
    if (!column) return;
    if (!(max >= 1)) { toast('The limit must be at least 1.'); return; }
    var limit = { column_id: column, max: max };
    if (limitEditIndex >= 0) editLaneLimits[limitEditIndex] = limit;
    else editLaneLimits.push(limit);
    document.getElementById('lane-limit-popup').hidden = true;
    renderLaneLimits();
  }

  function doSaveLane() {
    if (!editLaneId) return;
    var name = document.getElementById('le-name').value.trim();
    if (!name) { toast('Swimlane name is required.'); return; }
    document.getElementById('lane-edit-dialog').hidden = true;
    var description = document.getElementById('le-description').value;
    var patch = { name: name, limits: editLaneLimits };
    if (description !== editLaneDescription) patch.description = description.trim();
    patchLane(editLaneId, patch, 'Could not save the swimlane.');
  }


  function onLaneNameKeydown(e) {
    if (e.key !== 'Enter' || e.isComposing || e.keyCode === 229) return;
    e.preventDefault();
    doSaveLane();
  }

  function refreshLaneCounts() {
    document.querySelectorAll('[data-lane-count]').forEach(function (el) {
      var td = el.closest('td.board-cell');
      var list = td ? td.querySelector('.task-list') : null;
      if (!list) return;
      const count = list.querySelectorAll('.task-card:not(.task-card-epic)').length;
      var limit = el.dataset.limit ? parseInt(el.dataset.limit, 10) : null;
      var over = limit != null && count > limit;
      el.textContent = limit != null ? count + ' / ' + limit : (count > 0 ? String(count) : '');
      el.classList.toggle('lane-cellCount--over', over);
      if (over) el.title = '*LIMIT EXCEEDED*'; else el.removeAttribute('title');
    });
  }

  function initLaneResize() {
    document.addEventListener('mousedown', function (e) {
      var grip = e.target.closest('[data-lane-resize]');
      if (!grip || e.button !== 0) return;
      e.preventDefault();
      var row = grip.closest('tr');
      var cell = row ? row.querySelector('td.board-cell') : null;
      if (!cell) return;
      var startY = e.clientY;
      var startHeight = cell.offsetHeight;
      var height = startHeight;
      function move(ev) {
        height = Math.max(100, Math.min(4000, Math.round(startHeight + ev.clientY - startY)));
        row.style.setProperty('--lane-height', height + 'px');
      }
      function up() {
        document.removeEventListener('mousemove', move);
        document.removeEventListener('mouseup', up);
        if (height === startHeight) return;
        api('/api/swimlanes/' + encodeURIComponent(grip.getAttribute('data-lane-resize')), 'PATCH', { height: height })
          .then(drained)
          .then(function (res) { if (!res.ok) toast('Could not save the swimlane height.'); })
          .catch(function () { toast('Could not save the swimlane height.'); });
      }
      document.addEventListener('mousemove', move);
      document.addEventListener('mouseup', up);
    });
  }


  function moveTaskToDone(taskId) {
    var doneHeader = document.querySelector('.columnHeader[data-is-done="1"]');
    if (!doneHeader) return;
    var doneColId = doneHeader.dataset.columnId;
    var card = document.querySelector('.task-card[data-task-id="' + cssEscape(taskId) + '"]');
    var fromList = card ? card.closest('.task-list') : null;
    var swimlaneId = fromList ? (fromList.dataset.swimlaneId || null) : null;
    fetch('/api/tasks/' + encodeURIComponent(taskId) + '/move?force=true', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ column_id: doneColId, swimlane_id: swimlaneId, position: 0 }),
    }).then(drained).then(function (res) {
      if (res.ok) window.location.reload();
    });
  }

  // ---------- save board as template ----------

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

  var TASK_HASH = '#task-';

  function showTaskHash(taskId) {
    if (!boardId()) return;
    var hash = window.location.hash;
    if (hash && hash.indexOf(TASK_HASH) !== 0) return;
    var wanted = TASK_HASH + taskId;
    if (hash === wanted) return;
    window.history.replaceState(window.history.state, '', window.location.pathname + window.location.search + wanted);
  }

  function clearTaskHash() {
    if (window.location.hash.indexOf(TASK_HASH) !== 0) return;
    window.history.replaceState(window.history.state, '', window.location.pathname + window.location.search);
  }

  function openModal(taskId, keepDirty) {
    fetch('/api/tasks/' + encodeURIComponent(taskId) + '/modal', {
      headers: { 'Accept': 'text/html' },
    })
      .then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); })
      .then(function (html) {
        if (String(modalTaskId()) !== String(taskId)) modalTab = 'description';
        document.getElementById('modal-root').innerHTML = html;
        // A refresh must not clear unsaved-changes state (e.g. after a
        // labels/due-date save the board still needs a reload on close).
        modalDirty = !!keepDirty;
        modalSubviewOpen = false;
        // KF-250: lock the board behind the modal so it cannot scroll.
        document.body.classList.add('modal-open');
        showTaskHash(taskId);
        wireModal();
        if (document.querySelector('.board-wrap[data-role="viewer"]')) {
          document.querySelectorAll('#modal-root textarea, #modal-root input:not([type="search"])').forEach(function (field) {
            field.readOnly = true;
            if (field.type === 'checkbox') field.disabled = true;
          });
        }
      })
      .catch(function () { /* leave the board as-is on failure */ });
  }

  function refreshModal() {
    var id = modalTaskId();
    if (id) openModal(id, modalDirty);
  }

  var modalSubviewOpen = false;

  var MODAL_TABS = ['description', 'children', 'comments', 'subtasks', 'relations', 'files', 'history'];
  var modalTab = 'description';

  function selectModalTab(name) {
    if (MODAL_TABS.indexOf(name) === -1) return;
    if (!document.getElementById('tm-tab-' + name)) name = 'description';
    modalTab = name;
    Array.prototype.forEach.call(document.querySelectorAll('#modal-root [data-tm-tab]'), function (tab) {
      var on = tab.getAttribute('data-tm-tab') === name;
      tab.setAttribute('aria-selected', on ? 'true' : 'false');
      tab.tabIndex = on ? 0 : -1;
      if (on) tab.hidden = false;
    });
    MODAL_TABS.forEach(function (tabName) {
      var panel = document.getElementById('tm-panel-' + tabName);
      if (panel) panel.hidden = tabName !== name;
    });
    if (name === 'history') loadModalHistory();
    if (name === 'comments') fitOpenCommentBox();
  }

  const fitOpenCommentBox = () => {
    const box = document.getElementById('modal-comment-input');
    if (box) fitCommentBox(box);
  };

  function showModalTab(name) {
    if (modalSubviewOpen) {
      modalTab = name;
      var id = modalTaskId();
      if (id) openModal(id, true);
      return true;
    }
    selectModalTab(name);
    return false;
  }

  function loadModalHistory() {
    var id = modalTaskId();
    var host = document.getElementById('modal-history');
    if (!id || !host) return;
    fetch('/api/tasks/' + encodeURIComponent(id) + '/history-view', { headers: { 'Accept': 'text/html' } })
      .then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); })
      .then(function (html) { if (host.isConnected) host.innerHTML = html; })
      .catch(function () { toast('Could not load history.'); });
  }

  function wireModalTabs() {
    var all = Array.prototype.slice.call(document.querySelectorAll('#modal-root [data-tm-tab]'));
    all.forEach(function (tab) {
      tab.addEventListener('click', function () { showModalTab(tab.getAttribute('data-tm-tab')); });
      tab.addEventListener('keydown', function (e) {
        const tabs = all.filter((t) => !t.hidden);
        const i = tabs.indexOf(tab);
        var to = e.key === 'ArrowRight' ? i + 1 : e.key === 'ArrowLeft' ? i - 1 :
          e.key === 'Home' ? 0 : e.key === 'End' ? tabs.length - 1 : null;
        if (to === null || e.metaKey || e.ctrlKey || e.altKey) return;
        e.preventDefault();
        var next = tabs[(to + tabs.length) % tabs.length];
        showModalTab(next.getAttribute('data-tm-tab'));
        next.focus();
      });
    });
    selectModalTab(modalTab);
  }

  var LINK_PICKER_DEBOUNCE_MS = 150;
  var EPICS_REFRESH_DEBOUNCE_MS = 150;
  var HIDE_DONE_CHILDREN_FOLD = 'children-done';
  var LinkKind = { CHILD: 'child', PARENT: 'parent' };
  var linkPicker = { mode: null, onPick: null, timer: null, seq: 0, opener: null, board: null };

  function linkError(message) {
    try {
      var body = JSON.parse(message);
      if (body && typeof body.message === 'string') message = body.message;
    } catch (e) {}
    var err = new Error(message);
    err.readable = true;
    return err;
  }

  function linkFailure(err, fallback) {
    return err && err.readable ? err.message : fallback;
  }

  function taskLinksUrl(taskId) {
    return '/api/tasks/' + encodeURIComponent(taskId) + '/links';
  }

  function findTaskLink(taskId, kind, targetId) {
    return api(taskLinksUrl(taskId), 'GET')
      .then(function (res) { return res.ok ? res.json() : Promise.reject(new Error('links')); })
      .then(function (links) {
        return links.filter(function (link) {
          return link.kind === kind && (targetId === undefined || link.target_id === targetId);
        })[0] || null;
      });
  }

  function deleteTaskLink(taskId, link) {
    return api(taskLinksUrl(taskId) + '/' + encodeURIComponent(link.id), 'DELETE')
      .then(drained)
      .then(function (res) { if (!res.ok) throw new Error('delete'); });
  }

  function addTaskLink(taskId, kind, targetId) {
    return api(taskLinksUrl(taskId), 'POST', { kind: kind, target_id: targetId })
      .then(function (res) {
        return res.text().then(function (text) {
          if (!res.ok) throw linkError(text || 'Could not link the task.');
        });
      });
  }

  function refreshAfterLinkChange() {
    modalDirty = true;
    refreshModal();
  }

  function unlinkChild(childId) {
    var id = modalTaskId();
    if (!id) return;
    findTaskLink(id, LinkKind.CHILD, childId)
      .then(function (link) {
        if (!link) throw new Error('missing');
        return deleteTaskLink(id, link);
      })
      .then(refreshAfterLinkChange)
      .catch(function () { toast('Could not unlink the child.'); });
  }

  function removeFromUmbrella() {
    var id = modalTaskId();
    if (!id) return;
    findTaskLink(id, LinkKind.PARENT)
      .then(function (link) {
        if (!link) throw new Error('missing');
        return deleteTaskLink(id, link);
      })
      .then(function () {
        refreshAfterLinkChange();
        toast('Removed from the umbrella.');
      })
      .catch(function () { toast('Could not remove the task from its umbrella.'); });
  }

  function pickChild(targetId) {
    var id = modalTaskId();
    if (!id) return;
    addTaskLink(id, LinkKind.CHILD, targetId)
      .then(function () {
        modalTab = 'children';
        refreshAfterLinkChange();
      })
      .catch(function (err) { toast(linkFailure(err, 'Could not link the child.')); });
  }

  var newChildBusy = false;

  function closeNewChild(row, open) {
    var input = row.querySelector('[data-new-child-input]');
    input.value = '';
    row.hidden = true;
    open.focus();
  }

  function createNewChild(row) {
    if (newChildBusy) return;
    var modal = document.querySelector('#modal-root .task-modal');
    var input = row.querySelector('[data-new-child-input]');
    var name = input.value.trim();
    if (!modal || !name) { input.focus(); return; }
    var epicId = modal.dataset.taskId;
    var body = { column_id: modal.dataset.columnId, name: name };
    if (modal.dataset.swimlaneId) body.swimlane_id = modal.dataset.swimlaneId;
    newChildBusy = true;
    var created = null;
    fetch('/api/tasks', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
      body: JSON.stringify(body),
    })
      .then(function (res) {
        return res.text().then(function (text) {
          if (!res.ok) throw linkError(text || 'Could not create the card.');
          return JSON.parse(text);
        });
      })
      .then(function (task) {
        created = task;
        return addTaskLink(epicId, LinkKind.CHILD, task.id);
      })
      .then(function () {
        newChildBusy = false;
        modalTab = 'children';
        refreshAfterLinkChange();
      })
      .catch(function (err) {
        newChildBusy = false;
        if (!created) {
          toast(linkFailure(err, 'Could not create the card.'));
          return;
        }
        toast('Created "' + name + '" but could not add it to this epic. It is in the same column.');
        refreshAfterLinkChange();
      });
  }

  function wireNewChild(overlay) {
    var row = overlay.querySelector('[data-new-child-row]');
    var open = overlay.querySelector('[data-new-child-open]');
    if (!row || !open) return;
    var input = row.querySelector('[data-new-child-input]');
    open.addEventListener('click', function () {
      row.hidden = false;
      input.focus();
    });
    row.querySelector('[data-new-child-create]').addEventListener('click', function () { createNewChild(row); });
    row.querySelector('[data-new-child-cancel]').addEventListener('click', function () { closeNewChild(row, open); });
    input.addEventListener('keydown', function (e) {
      e.stopPropagation();
      if (e.key === 'Enter') {
        e.preventDefault();
        createNewChild(row);
      } else if (e.key === 'Escape') {
        e.preventDefault();
        closeNewChild(row, open);
      }
    });
  }

  function changeUmbrella(targetId) {
    var id = modalTaskId();
    if (!id) return;
    api('/api/tasks/' + encodeURIComponent(id) + '/umbrella', 'PUT', { target_id: targetId })
      .then(function (res) {
        return res.text().then(function (text) {
          if (!res.ok) throw linkError(text || 'Could not change the umbrella.');
        });
      })
      .then(function () {
        refreshAfterLinkChange();
        toast('Umbrella changed.');
      })
      .catch(function (err) {
        refreshAfterLinkChange();
        toast(linkFailure(err, 'Could not change the umbrella.'));
      });
  }

  function paintHideDoneChildren(button, hide) {
    button.setAttribute('aria-pressed', hide ? 'true' : 'false');
    button.textContent = hide ? 'Show done' : 'Hide done';
    var rows = Array.prototype.slice.call(document.querySelectorAll('#tm-children-list .tm-child'));
    var open = 0;
    rows.forEach(function (row) {
      var done = row.getAttribute('data-child-done') === 'true';
      row.hidden = hide && done;
      if (!done) open += 1;
    });
    var note = document.getElementById('tm-children-note');
    if (note) note.hidden = !(hide && rows.length > 0 && open === 0);
  }

  function toggleHideDoneChildren(button) {
    var hide = button.getAttribute('aria-pressed') !== 'true';
    paintHideDoneChildren(button, hide);
    loadSidebarFolds().then(function (loaded) {
      if (!loaded) { toast('Could not save the Hide done choice.'); return; }
      var at = sidebarFolds.groups.indexOf(HIDE_DONE_CHILDREN_FOLD);
      if (hide && at === -1) sidebarFolds.groups.push(HIDE_DONE_CHILDREN_FOLD);
      else if (!hide && at !== -1) sidebarFolds.groups.splice(at, 1);
      saveSidebarFolds();
    });
  }

  function loadLinkPickerResults(query) {
    var id = modalTaskId();
    var results = document.getElementById('tm-picker-results');
    if (!id || !results) return;
    linkPicker.seq += 1;
    var seq = linkPicker.seq;
    var terms = (linkPicker.board ? 'board:' + linkPicker.board.id + ' ' : '') + query;
    fetch('/api/tasks/' + encodeURIComponent(id) + '/link-picker?mode=' + encodeURIComponent(linkPicker.mode) +
        '&q=' + encodeURIComponent(terms), { headers: { 'Accept': 'text/html' } })
      .then(function (res) {
        if (res.ok) return res.text();
        if (res.status !== 400) return Promise.reject(res.status);
        return res.text().then(function (text) {
          var message = text;
          try {
            var body = JSON.parse(text);
            if (body && body.message) message = body.message;
          } catch (e) {}
          return Promise.reject(message || res.status);
        });
      })
      .then(function (html) {
        if (seq === linkPicker.seq && results.isConnected) results.innerHTML = html;
      })
      .catch(function (reason) {
        if (seq === linkPicker.seq && results.isConnected) {
          results.textContent = typeof reason === 'string' ? reason : 'Could not load tasks.';
        }
      });
  }

  function paintPickerChip() {
    var chip = document.getElementById('tm-picker-chip');
    if (!chip) return;
    chip.hidden = !linkPicker.board;
    chip.textContent = linkPicker.board ? 'board:' + linkPicker.board.name : '';
  }

  function dropPickerBoard() {
    var input = document.getElementById('tm-picker-input');
    if (!linkPicker.board || !input) return;
    linkPicker.board = null;
    paintPickerChip();
    input.focus();
    loadLinkPickerResults(input.value.trim());
  }

  function openLinkPicker(mode, onPick) {
    var picker = document.getElementById('tm-picker');
    var input = document.getElementById('tm-picker-input');
    if (!picker || !input) return;
    linkPicker.mode = mode;
    linkPicker.onPick = onPick;
    linkPicker.opener = document.activeElement;
    input.value = '';
    var current = boardId();
    var title = document.querySelector('.board-bar-title');
    linkPicker.board = modalTaskBoard() ||
      (current ? { id: current, name: title ? title.textContent.trim() : current } : null);
    paintPickerChip();
    picker.hidden = false;
    input.focus();
    loadLinkPickerResults('');
  }

  function closeLinkPicker(restoreFocus) {
    var picker = document.getElementById('tm-picker');
    if (!picker || picker.hidden) return false;
    picker.hidden = true;
    window.clearTimeout(linkPicker.timer);
    linkPicker.seq += 1;
    linkPicker.onPick = null;
    var opener = linkPicker.opener;
    linkPicker.opener = null;
    if (restoreFocus && opener && opener.isConnected && opener.focus) opener.focus();
    return true;
  }

  function wireLinkPicker(overlay) {
    var picker = overlay.querySelector('#tm-picker');
    var input = overlay.querySelector('#tm-picker-input');
    var results = overlay.querySelector('#tm-picker-results');
    if (!picker || !input || !results) return;
    input.addEventListener('input', function () {
      window.clearTimeout(linkPicker.timer);
      linkPicker.timer = window.setTimeout(function () {
        loadLinkPickerResults(input.value.trim());
      }, LINK_PICKER_DEBOUNCE_MS);
    });
    function items() {
      return Array.prototype.slice.call(results.querySelectorAll('[data-pick-id]'));
    }
    var chip = overlay.querySelector('#tm-picker-chip');
    if (chip) chip.addEventListener('click', dropPickerBoard);
    input.addEventListener('keydown', function (e) {
      if (e.key === 'Backspace' && input.value === '' && linkPicker.board) {
        e.preventDefault();
        dropPickerBoard();
        return;
      }
      if (e.key !== 'ArrowDown') return;
      var first = items()[0];
      if (!first) return;
      e.preventDefault();
      first.focus();
    });
    results.addEventListener('keydown', function (e) {
      if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
      var all = items();
      var at = all.indexOf(document.activeElement);
      if (at === -1) return;
      e.preventDefault();
      if (e.key === 'ArrowDown') {
        if (all[at + 1]) all[at + 1].focus();
      } else if (at === 0) {
        input.focus();
      } else {
        all[at - 1].focus();
      }
    });
    results.addEventListener('click', function (e) {
      var item = e.target.closest('[data-pick-id]');
      if (!item) return;
      var onPick = linkPicker.onPick;
      closeLinkPicker(false);
      if (onPick) onPick(item.getAttribute('data-pick-id'));
    });
  }

  function wireChildren(overlay) {
    var chip = overlay.querySelector('[data-children-chip]');
    if (chip) chip.addEventListener('click', function () { showModalTab('children'); });
    var hideDone = overlay.querySelector('[data-children-hide-done]');
    if (hideDone) hideDone.addEventListener('click', function () { toggleHideDoneChildren(hideDone); });
    Array.prototype.forEach.call(overlay.querySelectorAll('[data-link-open="child"]'), function (btn) {
      btn.addEventListener('click', function () { openLinkPicker(LinkKind.CHILD, pickChild); });
    });
    wireNewChild(overlay);
    Array.prototype.forEach.call(overlay.querySelectorAll('[data-unlink-child]'), function (btn) {
      btn.addEventListener('click', function () { unlinkChild(btn.getAttribute('data-unlink-child')); });
    });
    var change = overlay.querySelector('[data-umbrella-change]');
    if (change) change.addEventListener('click', function () { openLinkPicker(LinkKind.PARENT, changeUmbrella); });
    var remove = overlay.querySelector('[data-umbrella-remove]');
    if (remove) remove.addEventListener('click', removeFromUmbrella);
    wireLinkPicker(overlay);
  }

  function openRelatedTask(el) {
    var taskId = el.getAttribute('data-open-task');
    var other = el.getAttribute('data-open-board');
    if (other && modalBoardId() && other !== modalBoardId()) {
      window.location.href = '/b/' + encodeURIComponent(other) + '#task-' + encodeURIComponent(taskId);
      return;
    }
    openModal(taskId, modalDirty);
  }

  function deleteRelation(linkId) {
    var id = modalTaskId();
    if (!id) return;
    deleteTaskLink(id, { id: linkId })
      .then(function () {
        refreshAfterLinkChange();
        toast('Relation deleted.');
      })
      .catch(function () { toast('Could not delete the relation.'); });
  }

  function changeRelationType(linkId, targetId, kind, oldKind) {
    var id = modalTaskId();
    if (!id) return;
    deleteTaskLink(id, { id: linkId })
      .then(function () {
        return addTaskLink(id, kind, targetId).catch(function (err) {
          return addTaskLink(id, oldKind, targetId).then(function () { throw err; });
        });
      })
      .then(function () {
        refreshAfterLinkChange();
        toast('Relation changed.');
      })
      .catch(function (err) {
        refreshAfterLinkChange();
        toast(linkFailure(err, 'Could not change the relation.'));
      });
  }

  function showOwnerPanelError(boxId, message) {
    const box = document.getElementById(boxId);
    if (!box) return;
    box.textContent = message || '';
    box.hidden = !message;
  }

  function ownerPanelSend(spec) {
    const id = modalTaskId();
    if (!id) return Promise.resolve();
    const headers = { 'Content-Type': 'application/json' };
    if (spec.fence) headers['If-Owner-Fence'] = spec.fence;
    return fetch('/api/tasks/' + encodeURIComponent(id) + spec.path, {
      method: spec.method,
      headers,
      body: spec.data === undefined ? undefined : JSON.stringify(spec.data),
    })
      .then((res) => {
        if (res.ok) {
          modalDirty = true;
          refreshModal();
          return undefined;
        }
        return res.text().then((text) => {
          showOwnerPanelError(spec.boxId, errorMessage(text) || spec.failure);
        });
      })
      .catch(() => { showOwnerPanelError(spec.boxId, spec.failure); });
  }

  function ownerPanelSpec(button) {
    const act = button.getAttribute('data-owner-act');
    const seat = button.getAttribute('data-seat');
    const ttl = button.getAttribute('data-ttl');
    const take = { take_over: button.getAttribute('data-over') === '1' };
    if (ttl) take.ttl_seconds = Number(ttl);
    if (act === 'owner-take') {
      return { path: '/owner', method: 'POST', data: take, boxId: 'tm-owner-error', failure: 'Could not take the card.' };
    }
    if (act === 'release') {
      return {
        path: '/owner/release', method: 'POST', data: {}, fence: button.getAttribute('data-fence'),
        boxId: 'tm-owner-error', failure: 'Could not release the card.',
      };
    }
    if (act === 'seat-take') {
      take.seat = seat;
      return { path: '/seats/take', method: 'POST', data: take, boxId: 'tm-seat-error', failure: 'Could not take the seat.' };
    }
    if (act === 'ask-again') {
      return { path: '/seats/ask-again', method: 'POST', data: {}, boxId: 'tm-seat-error', failure: 'Could not ask again.' };
    }
    if (act === 'seat-add') {
      return {
        path: '/seats', method: 'POST', boxId: 'tm-seat-error', failure: 'Could not add the seat.',
        data: {
          name: document.getElementById('tm-seat-name').value.trim(),
          role: document.getElementById('tm-seat-role').value,
        },
      };
    }
    if (act === 'seat-drop') {
      return {
        path: '/seats?name=' + encodeURIComponent(seat), method: 'DELETE',
        boxId: 'tm-seat-error', failure: 'Could not drop the seat.',
      };
    }
    return null;
  }

  function runOwnerPanelAction(button) {
    const spec = ownerPanelSpec(button);
    if (!spec) return Promise.resolve();
    if (button.getAttribute('data-live-agent') === '1') {
      showConfirmDialog('Take over', 'An agent holds this with a live lease. Take it over anyway?', 'Take over',
        () => { ownerPanelSend(spec); });
      return Promise.resolve();
    }
    return ownerPanelSend(spec);
  }

  function wireOwnerPanel(overlay) {
    overlay.querySelectorAll('[data-owner-act]').forEach((button) => {
      button.addEventListener('click', () => { runOwnerPanelAction(button); });
    });
  }

  function showMetadataError(message) {
    var box = document.getElementById('tm-metadata-error');
    if (!box) return;
    box.textContent = message || '';
    box.hidden = !message;
  }

  function metadataPatch(before, after) {
    var patch = {};
    Object.keys(before).forEach(function (key) {
      if (!Object.prototype.hasOwnProperty.call(after, key)) patch[key] = null;
    });
    Object.keys(after).forEach(function (key) {
      if (JSON.stringify(before[key]) !== JSON.stringify(after[key])) patch[key] = after[key];
    });
    return patch;
  }

  function saveMetadata(id, input) {
    var before;
    var after;
    try {
      before = JSON.parse(input.defaultValue);
      after = JSON.parse(input.value);
    } catch (e) {
      showMetadataError('Not valid JSON: ' + e.message);
      return;
    }
    if (after === null || typeof after !== 'object' || Array.isArray(after)) {
      showMetadataError('Metadata must be a JSON object.');
      return;
    }
    api('/api/tasks/' + encodeURIComponent(id), 'PATCH', { metadata: metadataPatch(before, after) })
      .then(function (res) {
        if (res.ok) {
          modalDirty = true;
          refreshModal();
          return;
        }
        return res.text().then(function (text) {
          showMetadataError(errorMessage(text) || 'Could not save the metadata.');
        });
      })
      .catch(function () { showMetadataError('Could not save the metadata.'); });
  }

  function wireMetadata(overlay) {
    var wrap = overlay.querySelector('#modal-metadata-wrap');
    if (!wrap) return;
    var editor = wrap.querySelector('#tm-metadata-editor');
    var edit = wrap.querySelector('[data-metadata-edit]');
    edit.addEventListener('click', function () {
      showConfirmDialog('Edit metadata',
        'Agents and tools write this metadata and may depend on it. Changing it by hand can break them. Edit it only if you know what reads it.',
        'Edit', function () {
          editor.hidden = false;
          edit.hidden = true;
          wrap.querySelector('#tm-metadata-input').focus();
        });
    });
    wrap.querySelector('[data-metadata-cancel]').addEventListener('click', function () { refreshModal(); });
    wrap.querySelector('[data-metadata-save]').addEventListener('click', function () {
      var id = modalTaskId();
      if (id) saveMetadata(id, wrap.querySelector('#tm-metadata-input'));
    });
  }

  function wireRelations(overlay) {
    Array.prototype.forEach.call(overlay.querySelectorAll('[data-relation-delete]'), function (btn) {
      btn.addEventListener('click', function () { deleteRelation(btn.getAttribute('data-relation-delete')); });
    });
    Array.prototype.forEach.call(overlay.querySelectorAll('[data-relation-edit]'), function (btn) {
      btn.addEventListener('click', function () {
        closeAllTmMenus();
        btn.closest('.tm-relation').querySelector('.tm-relation-edit').hidden = false;
      });
    });
    Array.prototype.forEach.call(overlay.querySelectorAll('[data-relation-cancel]'), function (btn) {
      btn.addEventListener('click', function () { btn.closest('.tm-relation-edit').hidden = true; });
    });
    Array.prototype.forEach.call(overlay.querySelectorAll('[data-relation-save]'), function (btn) {
      btn.addEventListener('click', function () {
        var row = btn.closest('.tm-relation');
        var kind = row.querySelector('.tm-relation-select').value;
        if (kind === row.getAttribute('data-relation-kind')) {
          btn.closest('.tm-relation-edit').hidden = true;
          return;
        }
        changeRelationType(btn.getAttribute('data-relation-save'), btn.getAttribute('data-relation-target'), kind,
          row.getAttribute('data-relation-kind'));
      });
    });
  }

  var RELATION_MIN_LETTERS = 3;
  var RELATION_RESULT_LIMIT = 20;
  var relationDialog = { seq: 0, tasks: [], loaded: false, boardId: null };

  function relationNode(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function fetchBoardTasks(boardId) {
    var all = [];
    function page(cursor) {
      var url = '/api/tasks?limit=1000&board_id=' + encodeURIComponent(boardId) +
        (cursor ? '&cursor=' + encodeURIComponent(cursor) : '');
      return fetchJson(url).then(function (body) {
        all = all.concat(body.items || []);
        return body.next_cursor ? page(body.next_cursor) : all;
      });
    }
    return page(null);
  }

  function paintRelationResults(content) {
    var results = document.getElementById('rel-results');
    results.textContent = '';
    results.appendChild(content);
  }

  function renderRelationResults() {
    var query = document.getElementById('rel-search').value.trim().toLowerCase();
    if (query.length < RELATION_MIN_LETTERS) {
      paintRelationResults(relationNode('p', 'empty-note', 'Type at least ' + RELATION_MIN_LETTERS + ' letters to search.'));
      return;
    }
    if (!relationDialog.loaded) {
      paintRelationResults(relationNode('p', 'empty-note', 'Loading...'));
      return;
    }
    var own = modalTaskId();
    var matches = relationDialog.tasks.filter(function (task) {
      return task.id !== own && task.name.toLowerCase().indexOf(query) !== -1;
    });
    if (!matches.length) {
      paintRelationResults(relationNode('p', 'empty-note', 'No results found.'));
      return;
    }
    var list = relationNode('ul', 'tm-picker-list');
    matches.slice(0, RELATION_RESULT_LIMIT).forEach(function (task) {
      var button = relationNode('button', 'tm-picker-item');
      button.type = 'button';
      button.setAttribute('data-rel-pick', task.id);
      button.appendChild(relationNode('span', 'tm-picker-name', task.name));
      var item = relationNode('li');
      item.appendChild(button);
      list.appendChild(item);
    });
    var box = relationNode('div');
    box.appendChild(list);
    if (matches.length > RELATION_RESULT_LIMIT) {
      box.appendChild(relationNode('p', 'tm-picker-more', 'More tasks match. Keep typing to narrow the list.'));
    }
    paintRelationResults(box);
  }

  function loadRelationTasks(boardId) {
    relationDialog.seq += 1;
    var seq = relationDialog.seq;
    relationDialog.boardId = boardId;
    relationDialog.loaded = false;
    relationDialog.tasks = [];
    renderRelationResults();
    fetchBoardTasks(boardId)
      .then(function (tasks) {
        if (seq !== relationDialog.seq) return;
        relationDialog.tasks = tasks;
        relationDialog.loaded = true;
        renderRelationResults();
      })
      .catch(function () {
        if (seq !== relationDialog.seq) return;
        paintRelationResults(relationNode('p', 'empty-note', 'Could not load tasks.'));
      });
  }

  function openRelationDialog() {
    var overlay = document.getElementById('relation-dialog');
    var dlg = overlay.querySelector('.dlg');
    var boardSel = document.getElementById('rel-board');
    var search = document.getElementById('rel-search');
    var here = modalBoardId();
    search.value = '';
    document.getElementById('rel-kind').selectedIndex = 0;
    boardSel.textContent = '';
    var current = relationNode('option', '', document.title.replace(/ — Pomb$/, ''));
    current.value = here;
    boardSel.appendChild(current);
    loadRelationTasks(here);
    overlay.hidden = false;
    var add = document.querySelector('.task-modal-iconbar [data-tm-menu="tm-add-menu"]');
    var circle = add ? add.querySelector('.tm-icon') : null;
    var a = circle ? circle.getBoundingClientRect() : { right: window.innerWidth - dlg.offsetWidth - 8, top: 49 };
    dlg.style.left = Math.max(0, Math.min(a.right + 9.5, window.innerWidth - dlg.offsetWidth)) + 'px';
    dlg.style.top = Math.max(0, Math.min(a.top - 20, window.innerHeight - dlg.offsetHeight)) + 'px';
    search.focus();
    fetchJson('/api/boards').then(function (boards) {
      if (overlay.hidden) return;
      boardSel.textContent = '';
      (boards || []).forEach(function (b) {
        var option = relationNode('option', '', b.name);
        option.value = b.id;
        boardSel.appendChild(option);
      });
      if (!(boards || []).some(function (b) { return b.id === here; })) boardSel.appendChild(current);
      boardSel.value = relationDialog.boardId;
    }).catch(function () { toast('Could not load the boards.'); });
  }

  function pickRelation(targetId) {
    var id = modalTaskId();
    if (!id) return;
    addTaskLink(id, document.getElementById('rel-kind').value, targetId)
      .then(function () {
        document.getElementById('relation-dialog').hidden = true;
        refreshAfterLinkChange();
        toast('Relation added.');
      })
      .catch(function (err) { toast(linkFailure(err, 'Could not add the relation.')); });
  }

  function initRelationDialog() {
    var overlay = document.getElementById('relation-dialog');
    document.getElementById('rel-search').addEventListener('input', renderRelationResults);
    document.getElementById('rel-board').addEventListener('change', function (e) {
      loadRelationTasks(e.target.value);
    });
    document.getElementById('rel-results').addEventListener('click', function (e) {
      var item = e.target.closest('[data-rel-pick]');
      if (item) pickRelation(item.getAttribute('data-rel-pick'));
    });
    overlay.addEventListener('click', function (e) {
      if (e.target === e.currentTarget) e.currentTarget.hidden = true;
    });
  }

  function openTimeLogView() {
    var id = modalTaskId();
    if (!id) return;
    closeAllTmMenus();
    fetch('/api/tasks/' + encodeURIComponent(id) + '/time-log-view', { headers: { 'Accept': 'text/html' } })
      .then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); })
      .then(function (html) {
        var body = document.querySelector('#modal-root .task-modal-body');
        if (body) {
          body.innerHTML = html;
          modalSubviewOpen = true;
          modalDirty = true;
        }
      })
      .catch(function () { toast('Could not load time log.'); });
  }

  function openTaskTimeInColumn() {
    var id = modalTaskId();
    var board = modalBoardId();
    if (!id || !board) return;
    fetch('/api/boards/' + encodeURIComponent(board) + '/export-data', { headers: { 'Accept': 'application/json' } })
      .then(function (res) { return res.ok ? res.json() : Promise.reject(res.status); })
      .then(function (data) {
        var task = (data.tasks || []).filter(function (t) { return String(t.id) === String(id); })[0];
        if (!task) return Promise.reject('missing');
        var body = document.querySelector('#modal-root .task-modal-body');
        if (!body) return;
        var name = (document.getElementById('modal-name') || {}).value || task.name;
        var now = Date.now();
        var rows = TimeInColumnPage.taskRows(data, task, now);
        var html = '<div class="tm-subview" data-task-id="' + escapeHtml(id) + '">' +
          '<button type="button" class="tm-subview-back" data-tm-subview="back" title="Back to task">&larr; ' + escapeHtml(name) + '</button>' +
          '<h3 class="tm-subview-title">Time in column</h3>' +
          '<div class="tm-tic-export">' +
          '<select data-tm-tic-kind aria-label="Export value">' + TimeInColumnPage.exportKindOptions() + '</select> ' +
          '<select data-tm-tic-format aria-label="Export format">' + TimeInColumnPage.exportFormatOptions() + '</select> ' +
          '<button type="button" data-tm-tic-export>Export</button></div>';
        if (!rows.length) {
          html += '<p class="empty-note">This task has not been in a column yet.</p>';
        } else {
          html += '<table class="bshell-table tm-tic-table"><thead><tr><th scope="col">Column</th>' +
            '<th scope="col" class="num">Time in column</th></tr></thead><tbody>';
          rows.forEach(function (r) {
            html += '<tr class="tm-tic-row"><th scope="row"><button type="button" class="tm-tic-toggle" aria-expanded="false" data-tm-tic="' +
              escapeHtml(r.id) + '">' + escapeHtml(r.name) + '</button></th><td class="num">' +
              escapeHtml(TimeInColumnPage.fmtDuration(r.ms, 'minutes')) + '</td></tr>' +
              '<tr class="tm-tic-detail" data-tm-tic-for="' + escapeHtml(r.id) + '" hidden><td colspan="2"><ul class="tm-tic-intervals">';
            r.intervals.forEach(function (iv) {
              html += '<li><span class="tm-tic-start">' + escapeHtml(new Date(iv.start).toLocaleString(undefined, { hour12: accountPrefs.hour12 })) +
                '</span> &ndash; <span class="tm-tic-end">' + (iv.open ? 'now' : escapeHtml(new Date(iv.end).toLocaleString(undefined, { hour12: accountPrefs.hour12 }))) +
                '</span> (' + escapeHtml(TimeInColumnPage.fmtDuration(iv.end - iv.start, 'minutes')) + ')</li>';
            });
            html += '</ul></td></tr>';
          });
          html += '</tbody></table>';
        }
        html += '</div>';
        body.innerHTML = html;
        modalSubviewOpen = true;
        modalDirty = true;
        body.querySelector('[data-tm-tic-export]').addEventListener('click', function () {
          var kind = body.querySelector('[data-tm-tic-kind]').value;
          TimeInColumnPage.saveExport(TimeInColumnPage.exportFile(
            TimeInColumnPage.taskExportRows(rows, kind, now),
            body.querySelector('[data-tm-tic-format]').value,
            TimeInColumnPage.exportStem('task-time-in-column', kind)));
        });
        Array.prototype.forEach.call(body.querySelectorAll('[data-tm-tic]'), function (btn) {
          btn.addEventListener('click', function () {
            var open = btn.getAttribute('aria-expanded') !== 'true';
            btn.setAttribute('aria-expanded', open ? 'true' : 'false');
            var detail = body.querySelector('[data-tm-tic-for="' + cssEscape(btn.getAttribute('data-tm-tic')) + '"]');
            if (detail) detail.hidden = !open;
          });
        });
      })
      .catch(function () { toast('Could not load time in column.'); });
  }

  // Refresh the dedicated time-log sub-view when it is open (the
  // compact inline log is gone; the Time spent row links to the sub-view).
  function refreshModalLog() {
    var id = modalTaskId();
    if (!id) return;
    if (modalSubviewOpen) openTimeLogView();
  }

  function closeModal() {
    var root = document.getElementById('modal-root');
    if (!root) return;
    root.innerHTML = '';
    modalSubviewOpen = false;
    modalTab = 'description';
    document.body.classList.remove('modal-open'); // KF-250: release the board scroll lock.
    clearTaskHash();
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

  // Table fields in the task dialog: one row per record, one typed cell per column.
  function tableControl(tag, type, label) {
    var el = document.createElement(tag);
    if (type) el.type = type;
    el.setAttribute('aria-label', label);
    return el;
  }

  function tableCellEditor(column, value) {
    var control;
    if (column.kind === 'bool') {
      control = tableControl('input', 'checkbox', column.label);
      control.className = 'tm-field-check';
      control.checked = value === true;
      control.dataset.set = value === undefined ? '' : '1';
      control.addEventListener('change', function () { control.dataset.set = '1'; });
    } else if (column.kind === 'select') {
      control = tableControl('select', null, column.label);
      control.className = 'tm-field-input';
      var blank = document.createElement('option');
      blank.value = '';
      blank.textContent = '—';
      control.appendChild(blank);
      var listed = column.options.slice();
      if (typeof value === 'string' && listed.indexOf(value) < 0) listed.push(value);
      listed.forEach(function (choice) {
        var option = document.createElement('option');
        option.value = choice;
        option.textContent = column.options.indexOf(choice) < 0 ? choice + ' (not an option)' : choice;
        control.appendChild(option);
      });
      control.value = typeof value === 'string' ? value : '';
    } else if (column.kind === 'multiselect') {
      control = document.createElement('span');
      control.className = 'tm-field-multi';
      control.setAttribute('role', 'group');
      control.setAttribute('aria-label', column.label);
      var held = Array.isArray(value) ? value : [];
      column.options.concat(held.filter(function (choice) {
        return column.options.indexOf(choice) < 0;
      })).forEach(function (choice) {
        var label = document.createElement('label');
        label.className = 'tm-field-choice';
        var box = document.createElement('input');
        box.type = 'checkbox';
        box.value = choice;
        box.checked = held.indexOf(choice) >= 0;
        label.appendChild(box);
        label.appendChild(document.createTextNode(' ' + choice));
        control.appendChild(label);
      });
    } else {
      control = tableControl('input', column.kind === 'number' ? 'number' : 'text', column.label);
      control.className = 'tm-field-input';
      if (column.kind === 'number') control.step = 'any';
      if (value !== undefined && value !== null) control.value = String(value);
    }
    return control;
  }

  function tableCellValue(column, control) {
    if (column.kind === 'bool') {
      var kept = column.required || control.checked || control.dataset.set;
      return kept ? control.checked : undefined;
    }
    if (column.kind === 'multiselect') {
      var picked = Array.prototype.map.call(
        control.querySelectorAll('input:checked'), function (box) { return box.value; });
      return picked.length ? picked : undefined;
    }
    if (control.value.trim() === '') return undefined;
    if (column.kind !== 'number') return control.value;
    var number = Number(control.value);
    if (!Number.isFinite(number)) {
      throw new Error(column.label + ': "' + control.value + '" is not a number');
    }
    return number;
  }

  function tableRowButton(action) {
    var button = document.createElement('button');
    button.type = 'button';
    button.className = 'kf-btn tm-table-btn';
    button.dataset.tableAct = action;
    button.textContent = { up: '↑', down: '↓', remove: '×' }[action];
    return button;
  }

  function numberTableRows(tbody) {
    var rows = tbody.rows;
    Array.prototype.forEach.call(rows, function (tr, i) {
      var n = i + 1;
      var up = tr.querySelector('[data-table-act="up"]');
      var down = tr.querySelector('[data-table-act="down"]');
      var remove = tr.querySelector('[data-table-act="remove"]');
      up.setAttribute('aria-label', 'Move row ' + n + ' up');
      up.disabled = i === 0;
      down.setAttribute('aria-label', 'Move row ' + n + ' down');
      down.disabled = i === rows.length - 1;
      remove.setAttribute('aria-label', 'Remove row ' + n);
    });
  }

  function tableRowEditor(columns, row) {
    var tr = document.createElement('tr');
    columns.forEach(function (column) {
      var td = document.createElement('td');
      td.dataset.label = column.label;
      td.appendChild(tableCellEditor(column, row[column.key]));
      tr.appendChild(td);
    });
    var actions = document.createElement('td');
    actions.className = 'tm-table-actions';
    actions.appendChild(tableRowButton('up'));
    actions.appendChild(tableRowButton('down'));
    actions.appendChild(tableRowButton('remove'));
    tr.appendChild(actions);
    return tr;
  }

  function buildTableField(host) {
    var columns = JSON.parse(host.dataset.columns || '[]');
    var rows = JSON.parse(host.dataset.rows || '[]');
    host.dataset.columns = '';
    host.dataset.rows = '';
    var table = document.createElement('table');
    table.className = 'tm-table';
    var head = table.createTHead().insertRow();
    columns.forEach(function (column) {
      var th = document.createElement('th');
      th.scope = 'col';
      th.textContent = column.label + (column.required ? ' *' : '');
      head.appendChild(th);
    });
    var spare = document.createElement('th');
    spare.className = 'tm-table-actions';
    spare.textContent = 'Actions';
    head.appendChild(spare);
    var tbody = table.createTBody();
    rows.forEach(function (row) { tbody.appendChild(tableRowEditor(columns, row)); });
    numberTableRows(tbody);
    var add = document.createElement('button');
    add.type = 'button';
    add.className = 'kf-btn tm-table-add';
    add.dataset.tableAct = 'add';
    add.textContent = 'Add row';
    host.appendChild(table);
    host.appendChild(add);
    host.tableColumns = columns;
    fitTableField(host);
  }

  var TABLE_COLUMN_MIN_PX = 128;
  var TABLE_ACTIONS_PX = 110;

  function fitTableField(host) {
    var needed = host.tableColumns.length * TABLE_COLUMN_MIN_PX + TABLE_ACTIONS_PX;
    host.classList.toggle('tm-table-stack', needed > host.clientWidth);
  }

  window.addEventListener('resize', function () {
    Array.prototype.forEach.call(
      document.querySelectorAll('#modal-fields-wrap .tm-field-table'), fitTableField);
  });

  function buildTableFields() {
    Array.prototype.forEach.call(
      document.querySelectorAll('#modal-fields-wrap .tm-field-table'), buildTableField);
  }

  function tableFieldValue(host) {
    var columns = host.tableColumns;
    var rows = Array.prototype.map.call(host.querySelectorAll('tbody tr'), function (tr) {
      var row = {};
      columns.forEach(function (column, i) {
        var value = tableCellValue(column, tr.cells[i].firstChild);
        if (value !== undefined) row[column.key] = value;
      });
      return row;
    });
    return rows.length ? rows : null;
  }

  function restoreTableRows(host, rows) {
    var tbody = host.querySelector('tbody');
    tbody.innerHTML = '';
    (rows || []).forEach(function (row) { tbody.appendChild(tableRowEditor(host.tableColumns, row)); });
    numberTableRows(tbody);
    host.querySelector('.tm-table-add').focus();
  }

  function onTableAction(event) {
    var button = event.target.closest && event.target.closest('#modal-fields-wrap [data-table-act]');
    if (!button) return;
    var host = button.closest('.tm-field-table');
    var tbody = host.querySelector('tbody');
    var tr = button.closest('tr');
    var act = button.dataset.tableAct;
    if (act === 'add') {
      var fresh = tableRowEditor(host.tableColumns, {});
      tbody.appendChild(fresh);
      numberTableRows(tbody);
      var first = fresh.querySelector('input, select');
      if (first) first.focus();
      return;
    }
    var before;
    try { before = tableFieldValue(host); } catch (e) { before = undefined; }
    if (act === 'up' && tr.previousElementSibling) {
      tbody.insertBefore(tr, tr.previousElementSibling);
    } else if (act === 'down' && tr.nextElementSibling) {
      tbody.insertBefore(tr.nextElementSibling, tr);
    } else if (act === 'remove') {
      var next = tr.nextElementSibling || tr.previousElementSibling;
      tr.remove();
      (next ? next.querySelector('[data-table-act="remove"]') : host.querySelector('.tm-table-add')).focus();
    }
    numberTableRows(tbody);
    if (act !== 'remove') {
      var enabled = button.disabled ? tr.querySelector('[data-table-act]:not(:disabled)') : button;
      enabled.focus();
    }
    saveModalFieldOf(host.closest('.tm-field'), function () {
      if (before !== undefined) restoreTableRows(host, before);
    });
  }
  document.addEventListener('click', onTableAction);

  function sideEl(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function fieldGroupOf(key) {
    var dot = key.indexOf('.');
    return dot === -1 ? '' : key.slice(0, dot);
  }

  function fieldCellText(value) {
    if (value === null || value === undefined) return '';
    if (Array.isArray(value)) return value.join(', ');
    if (value === true) return 'Yes';
    if (value === false) return '';
    return String(value);
  }

  function userFieldSummary(field) {
    var kind = field.dataset.fieldKind;
    try {
      if (kind === 'table') {
        var host = field.querySelector('.tm-field-table');
        var rows = tableFieldValue(host);
        if (!rows) return null;
        return {
          mode: 'cards',
          values: rows.map(function (row) {
            return host.tableColumns.map(function (column) {
              return fieldCellText(row[column.key]);
            }).filter(Boolean).join(' · ');
          }),
        };
      }
      var value = modalFieldValue(field);
      if (kind === 'multiselect') {
        var gone = Array.prototype.map.call(field.querySelectorAll('.tm-field-stale-item'), function (item) { return item.textContent; }).filter(Boolean);
        var shown = (value || []).concat(gone.map(function (item) { return item + ' (not an option)'; }));
        return shown.length ? { mode: 'chips', values: shown } : null;
      }
      var text = fieldCellText(value).trim();
      var picked = kind === 'select' ? field.querySelector('option.tm-field-stale:checked') : null;
      if (picked && text) text += ' (not an option)';
      if (!text) return null;
      return { mode: kind === 'multiline' ? 'code' : 'text', values: [text] };
    } catch (e) {
      return null;
    }
  }

  function fieldGroupModel(names) {
    var groups = [];
    function groupFor(name) {
      if (!names[name]) {
        names[name] = { name: name, shown: [], empty: [], editable: false };
        groups.push(names[name]);
      }
      return names[name];
    }
    Array.prototype.forEach.call(document.querySelectorAll('#modal-fields-wrap .tm-field'), function (field) {
      var group = groupFor(fieldGroupOf(field.dataset.fieldKey));
      var labelEl = field.querySelector('.tm-field-label');
      var label = labelEl && labelEl.firstChild ? labelEl.firstChild.textContent : field.dataset.fieldKey;
      var summary = userFieldSummary(field);
      group.editable = true;
      if (summary) {
        summary.label = label;
        group.shown.push(summary);
      } else {
        group.empty.push(field.dataset.required === 'true' ? label + ' *' : label);
      }
    });
    Array.prototype.forEach.call(document.querySelectorAll('#modal-system-fields-wrap dd[data-field-key]'), function (dd) {
      var group = groupFor(fieldGroupOf(dd.dataset.fieldKey));
      var term = dd.previousElementSibling;
      var label = term ? term.textContent : dd.dataset.fieldKey;
      var text = dd.textContent.trim();
      if (!text || text === '—') {
        group.empty.push(label);
      } else {
        group.shown.push({ label: label, mode: text.indexOf('\n') === -1 ? 'text' : 'code', values: [text] });
      }
    });
    return groups;
  }

  function fieldGroupSection(group, index) {
    var section = sideEl('section', 'tm-side-group');
    section.dataset.group = group.name;
    if (!group.shown.length && group.empty.length) {
      section.classList.add('tm-side-group-folded');
      section.title = group.empty.join(', ');
      section.appendChild(sideEl('span', 'tm-side-title', group.name ? 'Fields · ' + group.name : 'Fields'));
      section.appendChild(sideEl('span', 'tm-empty-line', group.empty.length + ' empty'));
      if (group.editable) {
        var open = sideEl('button', 'tm-add-link', 'Edit');
        open.type = 'button';
        open.dataset.editFields = group.name;
        section.appendChild(open);
      }
      return section;
    }
    var title = sideEl('h3', 'tm-side-title');
    var fold = sideEl('button', 'tm-fold', group.name ? 'Fields · ' + group.name : 'Fields');
    fold.type = 'button';
    fold.dataset.foldGroup = 'fields:' + group.name;
    fold.setAttribute('aria-expanded', 'true');
    fold.setAttribute('aria-controls', 'tm-body-fields-' + index);
    title.appendChild(fold);
    section.appendChild(title);
    var body = sideEl('div', 'tm-side-body');
    body.id = 'tm-body-fields-' + index;
    var rows = sideEl('dl', 'tm-props');
    group.shown.forEach(function (item) {
      if (item.mode === 'code' || item.mode === 'cards') {
        var block = sideEl('div', 'tm-field-block');
        block.appendChild(sideEl('span', 'tm-field-block-label', item.label));
        item.values.forEach(function (value) {
          var card = sideEl('div', 'tm-field-card');
          card.appendChild(sideEl(item.mode === 'code' ? 'code' : 'span', item.mode === 'code' ? 'tm-field-code' : 'tm-field-row', value));
          card.title = value;
          block.appendChild(card);
        });
        body.appendChild(block);
        return;
      }
      rows.appendChild(sideEl('dt', '', item.label));
      var dd = sideEl('dd');
      if (item.mode === 'chips') {
        item.values.forEach(function (value) { dd.appendChild(sideEl('span', 'tm-field-chip', value)); });
      } else {
        dd.textContent = item.values[0];
      }
      rows.appendChild(dd);
    });
    if (rows.children.length) body.appendChild(rows);
    if (group.empty.length) body.appendChild(sideEl('p', 'tm-empty-line', 'Empty: ' + group.empty.join(', ')));
    if (group.editable) {
      var edit = sideEl('button', 'tm-side-btn', 'Edit fields');
      edit.type = 'button';
      edit.dataset.editFields = group.name;
      body.appendChild(edit);
    }
    section.appendChild(body);
    return section;
  }

  function renderFieldGroups() {
    var host = document.getElementById('tm-field-groups');
    if (!host) return;
    host.textContent = '';
    fieldGroupModel({}).forEach(function (group, index) { host.appendChild(fieldGroupSection(group, index)); });
    applySidebarFolds();
  }

  var sidebarFolds = null;
  var sidebarFoldLoad = null;
  var sidebarFoldDirty = false;
  var sidebarFoldSaving = false;

  function loadSidebarFolds() {
    if (sidebarFolds) return Promise.resolve(true);
    if (sidebarFoldLoad) return sidebarFoldLoad;
    sidebarFoldLoad = fetch('/api/settings', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
      .then(function (r) { if (!r.ok) throw new Error('HTTP ' + r.status); return r.json(); })
      .then(function (s) {
        sidebarFolds = {
          groups: Array.isArray(s.modal_folded_groups) ? s.modal_folded_groups.slice() : [],
        };
        sidebarFoldLoad = null;
        return true;
      })
      .catch(function () {
        sidebarFoldLoad = null;
        return false;
      });
    return sidebarFoldLoad;
  }

  function saveSidebarFolds() {
    sidebarFoldDirty = true;
    if (sidebarFoldSaving) return;
    sidebarFoldSaving = true;
    function send() {
      if (!sidebarFolds || !sidebarFoldDirty) { sidebarFoldSaving = false; return; }
      sidebarFoldDirty = false;
      var body = JSON.stringify({ modal_folded_groups: sidebarFolds.groups });
      fetch('/api/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: body,
      }).then(function (r) {
        if (!r.ok) toast('Could not save the dialog layout.');
      }).catch(function () {
        toast('Could not save the dialog layout.');
      }).then(send);
    }
    send();
  }

  function paintGroupFold(button, folded) {
    button.setAttribute('aria-expanded', folded ? 'false' : 'true');
    var body = document.getElementById(button.getAttribute('aria-controls'));
    if (body) body.hidden = folded;
  }

  function paintPanelFold(folded) {
    var side = document.getElementById('tm-side');
    if (!side) return;
    side.hidden = folded;
    var hide = side.querySelector('[data-sidebar-hide]');
    if (hide) hide.setAttribute('aria-expanded', folded ? 'false' : 'true');
    var show = document.querySelector('#modal-root [data-sidebar-show]');
    if (show) {
      show.setAttribute('aria-expanded', folded ? 'false' : 'true');
      show.parentNode.hidden = !folded;
    }
  }

  function applySidebarFolds() {
    if (!sidebarFolds) return;
    paintPanelFold(sidebarFolds.groups.indexOf('panel') !== -1);
    var editor = document.getElementById('tm-group-edit-fields');
    Array.prototype.forEach.call(document.querySelectorAll('#modal-root [data-fold-group]'), function (button) {
      if (button.dataset.foldGroup === 'edit-fields' && editor && !editor.hidden) return;
      paintGroupFold(button, sidebarFolds.groups.indexOf(button.dataset.foldGroup) !== -1);
    });
  }

  function setGroupFolded(button, folded) {
    var key = button.dataset.foldGroup;
    paintGroupFold(button, folded);
    loadSidebarFolds().then(function (loaded) {
      if (!loaded) return;
      var at = sidebarFolds.groups.indexOf(key);
      if (folded && at === -1) sidebarFolds.groups.push(key);
      else if (!folded && at !== -1) sidebarFolds.groups.splice(at, 1);
      paintGroupFold(button, folded);
      saveSidebarFolds();
    });
  }

  function setPanelFolded(folded) {
    paintPanelFold(folded);
    var focus = document.querySelector(folded ? '#modal-root [data-sidebar-show]' : '#modal-root [data-sidebar-hide]');
    if (focus) focus.focus();
    loadSidebarFolds().then(function (loaded) {
      if (!loaded) return;
      var at = sidebarFolds.groups.indexOf('panel');
      if (folded && at === -1) sidebarFolds.groups.push('panel');
      else if (!folded && at !== -1) sidebarFolds.groups.splice(at, 1);
      paintPanelFold(folded);
      saveSidebarFolds();
    });
  }

  function wireSidebarFolds() {
    var layout = document.querySelector('#modal-root .tm-layout');
    if (!layout) return;
    layout.addEventListener('click', function (e) {
      var group = e.target.closest('[data-fold-group]');
      if (group) {
        if (group.getAttribute('aria-disabled') !== 'true') setGroupFolded(group, group.getAttribute('aria-expanded') === 'true');
        return;
      }
      if (e.target.closest('[data-sidebar-hide]')) setPanelFolded(true);
      else if (e.target.closest('[data-sidebar-show]')) setPanelFolded(false);
    });
    applySidebarFolds();
    loadSidebarFolds().then(applySidebarFolds);
  }

  function openFieldEditor(groupName) {
    var panel = document.getElementById('tm-group-edit-fields');
    if (!panel) return;
    Array.prototype.forEach.call(panel.querySelectorAll('.tm-field'), function (field) {
      field.hidden = fieldGroupOf(field.dataset.fieldKey) !== groupName;
    });
    document.getElementById('tm-edit-fields-title').textContent =
      groupName ? 'Edit fields · ' + groupName : 'Edit fields';
    panel.hidden = false;
    var fold = panel.querySelector('[data-fold-group]');
    if (fold) {
      paintGroupFold(fold, false);
      fold.setAttribute('aria-disabled', 'true');
      fold.title = 'Finish editing fields first';
    }
    Array.prototype.forEach.call(panel.querySelectorAll('.tm-field-table'), fitTableField);
    panel.scrollIntoView({ block: 'nearest' });
    var first = panel.querySelector('.tm-field:not([hidden]) .tm-field-input, .tm-field:not([hidden]) input');
    if (first) first.focus();
  }

  function closeFieldEditor() {
    var panel = document.getElementById('tm-group-edit-fields');
    if (panel) {
      panel.hidden = true;
      var fold = panel.querySelector('[data-fold-group]');
      if (fold) {
        fold.removeAttribute('aria-disabled');
        fold.removeAttribute('title');
      }
    }
    renderFieldGroups();
  }

  function wireFieldGroups() {
    var side = document.querySelector('#modal-root .tm-side');
    if (!side) return;
    renderFieldGroups();
    side.addEventListener('click', function (e) {
      var edit = e.target.closest('[data-edit-fields]');
      if (edit) { openFieldEditor(edit.dataset.editFields); return; }
      if (e.target.closest('[data-edit-fields-done]')) closeFieldEditor();
    });
  }

  // Custom fields in the task dialog: each change PATCHes that one key.
  function modalFieldValue(field) {
    var kind = field.dataset.fieldKind;
    if (kind === 'table') return tableFieldValue(field.querySelector('.tm-field-table'));
    if (kind === 'multiselect') {
      var picked = Array.prototype.map.call(
        field.querySelectorAll('input:checked'), function (box) { return box.value; });
      return picked.length ? picked : null;
    }
    var input = field.querySelector('.tm-field-input');
    if (kind === 'bool') return input.checked;
    if (input.value.trim() === '') return null;
    return kind === 'number' ? Number(input.value) : input.value;
  }

  function errorMessage(text) {
    try { return JSON.parse(text).message || text; } catch (e) { return text; }
  }

  function showFieldError(field, message) {
    var box = field.querySelector('.tm-field-error');
    box.textContent = message || '';
    box.hidden = !message;
  }

  function saveModalField(event) {
    var field = event.target.closest && event.target.closest('#modal-fields-wrap .tm-field');
    if (field) saveModalFieldOf(field);
  }

  function saveModalFieldOf(field, onRejected) {
    var id = modalTaskId();
    if (!id) return;
    var value;
    try {
      value = modalFieldValue(field);
    } catch (e) {
      showFieldError(field, e.message);
      return;
    }
    var patch = { fields: {} };
    patch.fields[field.dataset.fieldKey] = value;
    fetch('/api/tasks/' + encodeURIComponent(id), {
      method: 'PATCH',
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
      body: JSON.stringify(patch),
    }).then(function (res) {
      if (!res.ok && field.dataset.fieldKind === 'table') {
        return res.text().then(function (text) {
          if (onRejected) onRejected();
          showFieldError(field, errorMessage(text) || 'Could not save.');
        });
      }
      if (!res.ok) return res.text().then(function (text) { toast(text); });
      showFieldError(field, '');
      modalDirty = true;
      var empty = value === null || value === false && field.dataset.fieldKind !== 'bool';
      field.classList.toggle('tm-field-missing', field.dataset.required === 'true' && empty);
      // The saved value replaced any choice the options no longer list.
      field.classList.remove('tm-field-invalid');
      renderFieldGroups();
    });
  }
  document.addEventListener('change', saveModalField);

  document.addEventListener('change', function (e) {
    var toggle = e.target.closest ? e.target.closest('[data-tm-history-all]') : null;
    if (!toggle) return;
    var view = toggle.closest('.tm-subview');
    if (!view) return;
    var list = view.querySelector('.tm-history-list');
    if (list) list.classList.toggle('tm-history-all', toggle.checked);
    var link = view.querySelector('[data-tm-history-export]');
    if (link) {
      link.setAttribute('href', link.getAttribute('data-tm-history-export') + (toggle.checked ? '?all=1' : ''));
    }
  });

  var MD_VIEW_KEY = 'pomb.markdownView';
  function mdViewStored() {
    try { return window.localStorage.getItem(MD_VIEW_KEY) === 'raw' ? 'raw' : 'rendered'; }
    catch (e) { return 'rendered'; }
  }
  function syncMdToggle() {
    var raw = document.documentElement.dataset.mdView === 'raw';
    document.querySelectorAll('[data-md-toggle]').forEach(function (btn) {
      btn.setAttribute('aria-pressed', raw ? 'true' : 'false');
      btn.title = raw ? 'Show the rendered markdown' : 'Show the raw text';
    });
  }
  document.documentElement.dataset.mdView = mdViewStored();
  document.addEventListener('click', function (e) {
    var btn = e.target.closest('[data-md-toggle]');
    if (!btn) return;
    var next = document.documentElement.dataset.mdView === 'raw' ? 'rendered' : 'raw';
    document.documentElement.dataset.mdView = next;
    try { window.localStorage.setItem(MD_VIEW_KEY, next); } catch (err) {}
    syncMdToggle();
  });

  // KF-253: the description is a static text block (click-to-edit); edits
  // go through the description editor dialog, never an inline textarea.
  function openDescriptionEditor() {
    var id = modalTaskId();
    if (!id) return;
    var wrap = document.getElementById('modal-description-wrap');
    var input = document.getElementById('desc-editor-input');
    if (!wrap || !input) return;
    input.value = wrap.dataset.description || '';
    pastedImages = false;
    document.getElementById('description-dialog').hidden = false;
    input.focus();
  }

  var DESC_SKIP = 'a, button, input, select, textarea, label';
  function bindDescriptionBlock(block) {
    block.addEventListener('click', function (e) {
      if (e.target.closest(DESC_SKIP)) return;
      var sel = window.getSelection();
      if (sel && sel.toString() !== '') return;
      openDescriptionEditor();
    });
    block.addEventListener('keydown', function (e) {
      if (e.target.closest(DESC_SKIP)) return;
      if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); openDescriptionEditor(); }
    });
    block.addEventListener('change', function (e) {
      var box = e.target;
      if (box.matches && box.matches('input[data-checklist-index]')) tickChecklistItem(box);
    });
  }

  function reloadDescription(id) {
    return fetch('/api/tasks/' + encodeURIComponent(id) + '/modal', { headers: { 'Accept': 'text/html' } })
      .then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); })
      .then(swapDescription);
  }

  function checklistBoxes() {
    return document.querySelectorAll('#modal-description-text input[data-checklist-index]');
  }

  function setChecklistBusy(busy) {
    checklistBoxes().forEach(function (el) { el.disabled = busy; });
  }

  function tickChecklistItem(box) {
    var id = modalTaskId();
    var wrap = document.getElementById('modal-description-wrap');
    if (!id || !wrap) return;
    var wanted = box.checked;
    setChecklistBusy(true);
    api('/api/tasks/' + encodeURIComponent(id) + '/checklist/' + encodeURIComponent(box.dataset.checklistIndex),
        'POST', { checked: wanted, description_hash: wrap.dataset.descriptionHash || '' })
      .then(function (res) {
        if (res.status === 409) {
          toast('The description changed, so it was reloaded.');
          return reloadDescription(id).catch(refreshModal);
        }
        if (!res.ok) throw new Error('tick failed');
        modalDirty = true;
        return reloadDescription(id).catch(refreshModal);
      })
      .catch(function () {
        box.checked = !wanted;
        toast('Could not update the checklist item.');
      })
      .then(function () { setChecklistBusy(false); });
  }

  function swapDescription(html) {
    var doc = new DOMParser().parseFromString(html, 'text/html');
    var fresh = doc.getElementById('modal-description-text');
    var freshWrap = doc.getElementById('modal-description-wrap');
    var block = document.getElementById('modal-description-text');
    var wrap = document.getElementById('modal-description-wrap');
    if (!fresh || !freshWrap || !block || !wrap) { refreshModal(); return; }
    wrap.dataset.description = freshWrap.dataset.description;
    wrap.dataset.descriptionHash = freshWrap.dataset.descriptionHash;
    var next = document.importNode(fresh, true);
    block.replaceWith(next);
    bindDescriptionBlock(next);
    Array.prototype.forEach.call(doc.querySelectorAll('.tm-attachment[data-attachment-refs]'), function (fresh) {
      var row = document.querySelector('.tm-attachment[data-attachment-id="' + cssEscape(fresh.getAttribute('data-attachment-id')) + '"]');
      if (row) row.setAttribute('data-attachment-refs', fresh.getAttribute('data-attachment-refs'));
    });
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
        if (pastedImages) {
          pastedImages = false;
          var draft = document.getElementById('modal-comment-input');
          pendingCommentDraft = draft ? draft.value : null;
          refreshModal();
          toast('Description saved.');
          return Promise.resolve();
        }
        return reloadDescription(id)
          .then(function () { toast('Description saved.'); }, function () {
            toast('Saved, but the view could not refresh.');
          });
      }, function () { toast('Could not save description.'); });
  }

  function epicModalFacts() {
    var modal = document.querySelector('#modal-root .task-modal.tm-epic');
    if (!modal) return null;
    var nameInput = document.getElementById('modal-name');
    var done = parseInt(modal.dataset.childrenDone, 10) || 0;
    var total = parseInt(modal.dataset.childrenTotal, 10) || 0;
    return { name: nameInput ? nameInput.value : '', done: done, total: total, open: Math.max(total - done, 0) };
  }

  function epicRequest(path, method, body) {
    return fetch(path, {
      method: method,
      headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    }).then(function (res) {
      return res.text().then(function (text) {
        if (!res.ok) throw text ? linkError(text) : new Error('refused');
        return text;
      });
    });
  }

  function setEpicClosed(id, closed) {
    epicRequest('/api/tasks/' + encodeURIComponent(id) + (closed ? '/close' : '/reopen'), 'POST')
      .then(refreshAfterLinkChange)
      .catch(function (err) {
        toast(linkFailure(err, closed ? 'Could not close the epic.' : 'Could not reopen the epic.'));
      });
  }

  function closeModalEpic() {
    var id = modalTaskId();
    var epic = epicModalFacts();
    if (!id || !epic) return;
    if (epic.open === 0) { setEpicClosed(id, true); return; }
    showConfirmDialog('Close epic',
      epic.open + ' of ' + epic.total + ' cards are still open. Close anyway? They stay where they are.',
      'Close anyway', function () { setEpicClosed(id, true); });
  }

  function epicCardsPhrase(total) {
    return total === 1
      ? 'Its 1 card stays on the board and loses its link to it.'
      : 'Its ' + total + ' cards stay on the board and lose their link to it.';
  }

  function unmakeModalEpic() {
    var id = modalTaskId();
    var epic = epicModalFacts();
    if (!id || !epic) return;
    var message = 'Make "' + epic.name + '" an ordinary card?';
    if (epic.total > 0) message += ' ' + epicCardsPhrase(epic.total);
    showConfirmDialog('Make it a card', message, 'Make it a card', function () {
      epicRequest('/api/tasks/' + encodeURIComponent(id), 'PATCH', { kind: 'task', unlink_children: true })
        .then(refreshAfterLinkChange)
        .catch(function (err) { toast(linkFailure(err, 'Could not make the epic a card.')); });
    });
  }

  function showMakeEpicDialog(name, subs, onConfirm) {
    var overlay = document.getElementById('make-epic-dialog');
    var message = '"' + name + '" leaves its column and becomes an epic. It holds no work of its own.';
    if (subs.length) message += ' Subtasks you leave unticked stay stored, but an epic does not show them.';
    document.getElementById('make-epic-message').textContent = message;
    var list = document.getElementById('make-epic-list');
    list.textContent = '';
    var boxes = subs.map(function (sub) {
      var label = document.createElement('label');
      var box = document.createElement('input');
      var text = document.createElement('span');
      box.type = 'checkbox';
      box.checked = false;
      text.textContent = sub.name;
      label.appendChild(box);
      label.appendChild(text);
      list.appendChild(label);
      return { box: box, sub: sub };
    });
    document.getElementById('make-epic-subtasks').hidden = !subs.length;
    var okBtn = document.getElementById('make-epic-ok');
    var fresh = okBtn.cloneNode(true);
    okBtn.parentNode.replaceChild(fresh, okBtn);
    fresh.addEventListener('click', function () {
      overlay.hidden = true;
      onConfirm(boxes.filter(function (entry) { return entry.box.checked; }).map(function (entry) { return entry.sub; }));
    });
    overlay.hidden = false;
    fresh.focus();
  }

  function subtaskProblem(sub, linking, err) {
    var reason = linkFailure(err, '');
    return { name: sub.name, linking: linking, reason: reason };
  }

  function turnSubtaskIntoChild(epicId, sub) {
    var base = '/api/tasks/' + encodeURIComponent(epicId);
    return epicRequest(base + '/subtasks/' + encodeURIComponent(sub.id) + '/convert', 'POST', { relation: 'none' })
      .then(function (text) {
        return Promise.resolve()
          .then(function () {
            return epicRequest(base + '/links', 'POST', { kind: 'child', target_id: JSON.parse(text).id });
          })
          .then(function () { return null; }, function (err) { return subtaskProblem(sub, true, err); });
      }, function (err) { return subtaskProblem(sub, false, err); });
  }

  function makeEpicProblems(problems) {
    function listed(items) {
      return items.map(function (p) { return '"' + p.name + '"' + (p.reason ? ' (' + p.reason + ')' : ''); }).join(', ');
    }
    var notConverted = problems.filter(function (p) { return !p.linking; });
    var notLinked = problems.filter(function (p) { return p.linking; });
    var parts = [];
    if (notConverted.length) parts.push('These subtasks could not become cards and stay subtasks: ' + listed(notConverted) + '.');
    if (notLinked.length) parts.push('These became cards in the same column but are not in the epic: ' + listed(notLinked) + '.');
    return parts.join(' ');
  }

  function makeEpic(id, picked, finish) {
    epicRequest('/api/tasks/' + encodeURIComponent(id), 'PATCH', { kind: 'epic' })
      .then(function () {
        var problems = [];
        return picked.reduce(function (chain, sub) {
          return chain
            .then(function () { return turnSubtaskIntoChild(id, sub); })
            .then(function (problem) { if (problem) problems.push(problem); });
        }, Promise.resolve()).then(function () { finish(makeEpicProblems(problems)); });
      }, function (err) { toast(linkFailure(err, 'Could not make the card an epic.')); });
  }

  function startMakeEpic(id, name, finish) {
    fetchJson('/api/tasks/' + encodeURIComponent(id)).then(function (task) {
      var subs = task.subtasks || [];
      if (!subs.length) { makeEpic(id, [], finish); return; }
      showMakeEpicDialog(name, subs, function (picked) { makeEpic(id, picked, finish); });
    }, function () { toast('Could not load the card.'); });
  }

  function makeModalEpic(id) {
    var nameInput = document.getElementById('modal-name');
    startMakeEpic(id, nameInput ? nameInput.value : '', function (problems) {
      refreshAfterLinkChange();
      if (problems) toast(problems);
    });
  }

  function makeCardEpic(task) {
    if (!task) return;
    startMakeEpic(task.id, task.name, function (problems) {
      if (problems) {
        try { window.sessionStorage.setItem('pomb.pendingToast', problems); } catch (e) {}
      }
      window.location.reload();
    });
  }

  function epicLinkCall(taskId, epicId, hasParent) {
    if (hasParent) {
      return epicRequest('/api/tasks/' + encodeURIComponent(taskId) + '/umbrella', 'PUT', { target_id: epicId });
    }
    return epicRequest('/api/tasks/' + encodeURIComponent(epicId) + '/links', 'POST', { kind: 'child', target_id: taskId });
  }

  function addToEpic(taskId, epicId, hasParent, refresh) {
    epicLinkCall(taskId, epicId, hasParent).then(refresh, function (err) {
      toast(linkFailure(err, 'Could not add the card to the epic.'));
    });
  }

  function appendEpicChoices(container, choices, makeButton) {
    if (!choices.length) {
      var none = document.createElement('button');
      none.type = 'button';
      none.disabled = true;
      none.textContent = 'No open epics';
      container.appendChild(none);
      return;
    }
    choices.forEach(function (choice) {
      var button = makeButton((choice.depth ? '↳ ' : '') + choice.name, choice.id);
      if (choice.depth) button.style.setProperty('--depth', choice.depth);
      container.appendChild(button);
    });
  }

  function addCardToEpic(card, epicId) {
    var id = card.dataset.taskId;
    addToEpic(id, epicId, !!card.querySelector('.card-epic-tag'), function () {
      if (window.TimerUI && TimerUI.refreshTaskCard) TimerUI.refreshTaskCard(id);
      else window.location.reload();
    });
  }

  function toggleModalEpicList(trigger) {
    var list = document.getElementById('tm-epic-list');
    var id = modalTaskId();
    if (!list || !id) return;
    if (!list.hidden) {
      list.hidden = true;
      trigger.setAttribute('aria-expanded', 'false');
      return;
    }
    BoardChrome.epicChoices(id).then(function (choices) {
      if (!choices) { toast('Could not load epics.'); return; }
      list.textContent = '';
      appendEpicChoices(list, choices, function (label, epicId) {
        var button = document.createElement('button');
        button.type = 'button';
        button.textContent = label;
        button.addEventListener('click', function () {
          var hasParent = !!document.querySelector('#modal-root .tm-umbrella-line');
          closeAllTmMenus();
          addToEpic(id, epicId, hasParent, refreshAfterLinkChange);
        });
        return button;
      });
      list.hidden = false;
      trigger.setAttribute('aria-expanded', 'true');
    });
  }

  function deleteModalEpic(id, epic) {
    var message = 'Delete "' + epic.name + '"? It goes to the recycle bin.';
    if (epic.total > 0) {
      message += ' ' + epicCardsPhrase(epic.total) + ' Restoring the epic from the bin does not link them again.';
    }
    showConfirmDialog('Delete epic', message, 'Delete epic', function () {
      epicRequest('/api/tasks/' + encodeURIComponent(id), 'DELETE')
        .then(function () {
          modalDirty = true;
          closeModal();
          if (window.TimerUI) { window.TimerUI.refresh(); window.TimerUI.broadcastTimer(); }
        })
        .catch(function (err) { toast(linkFailure(err, 'Could not delete the epic.')); });
    });
  }

  function deleteModalTask() {
    var id = modalTaskId();
    if (!id) return;
    var epic = epicModalFacts();
    if (epic) { deleteModalEpic(id, epic); return; }
    // KF-184: styled in-page confirmation (KanbanFlow parity); the native
    // window.confirm() auto-dismisses under automation, silently no-op'ing.
    showConfirmDialog('Delete task', 'Delete this task? It stays in the Recycle bin for 30 days.', 'Delete', function () {
      api('/api/tasks/' + encodeURIComponent(id), 'DELETE')
        .then(drained)
        .then(function (res) {
          if (res.ok) {
            modalDirty = true;
            closeModal();
            if (window.TimerUI) { window.TimerUI.refresh(); window.TimerUI.broadcastTimer(); }
          }
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

  var BARE_KEEPING_ACTS = ['pick-color', 'move', 'delete', 'watch', 'task-url', 'copy', 'make-epic', 'shortcuts', 'print'];

  function modalAction(act, source) {
    var id = modalTaskId();
    if (act === 'add-to-epic') { toggleModalEpicList(source); return; }
    closeAllTmMenus();
    if (BARE_KEEPING_ACTS.indexOf(act) === -1) {
      var bare = document.querySelector('#modal-root .task-modal.tm-bare');
      if (bare) bare.classList.remove('tm-bare');
    }
    if (act === 'pick-color') {
      // KF-257: the Color row shows dot + color name; clicking the dot toggles the full picker.
      var picker = document.getElementById('modal-color-picker');
      if (picker) picker.hidden = !picker.hidden;
    } else if (act === 'manual-time') {
      var nameInput = document.getElementById('modal-name');
      ManualTime.open(id, nameInput ? nameInput.value : '');
    } else if (act === 'estimate') {
      openEstimateDialog();
    } else if (act === 'move') {
      openMoveDialog();
    } else if (act === 'start-pomodoro') {
      TimerUI.start('pomodoro', id);
    } else if (act === 'start-stopwatch') {
      TimerUI.start('stopwatch', id);
    } else if (act === 'time-log') {
      openTimeLogView();
    } else if (act === 'history') {
      showModalTab('history');
    } else if (act === 'print') {
      window.print();
    } else if (act === 'time-in-column') {
      openTaskTimeInColumn();
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
      openCopyDialog();
    } else if (act === 'shortcuts') {
      document.getElementById('shortcuts-dialog').hidden = false;
    } else if (act === 'delete') {
      deleteModalTask();
    } else if (act === 'close-epic') {
      closeModalEpic();
    } else if (act === 'reopen-epic') {
      setEpicClosed(id, false);
    } else if (act === 'make-card') {
      unmakeModalEpic();
    } else if (act === 'make-epic') {
      makeModalEpic(id);
    } else if (act === 'add-description') {
      // KF-253: opens the description editor dialog (the description is a
      // static text block now, never an inline textarea).
      if (showModalTab('description')) return;
      openDescriptionEditor();
    } else if (act === 'add-member') {
      openMembersDialog(id);
    } else if (act === 'add-label') {
      // KF-144: was openLabelsDialog() (undefined) — the dialog object is LabelsDialog.
      LabelsDialog.open(source);
    } else if (act === 'add-subtask') {
      if (showModalTab('subtasks')) return;
      var subInput = document.getElementById('modal-subtask-input');
      if (subInput) { subInput.focus(); subInput.scrollIntoView({ behavior: 'smooth', block: 'center' }); }
    } else if (act === 'add-timeline' || act === 'edit-timeline') {
      TimelineDialog.open();
    } else if (act === 'add-duedate') {
      // KF-145: was openDueDateDialog() (undefined) — the dialog object is DueDateDialog.
      DueDateDialog.open(source);
    } else if (act === 'due-done-toggle') {
      var wasDone = source.getAttribute('data-done') === 'true';
      api('/api/tasks/' + encodeURIComponent(id), 'PATCH', { due_done: !wasDone }).then(function (res) {
        if (!res.ok) throw new Error('save failed');
        modalDirty = true;
        openModal(id, true);
        toast(wasDone ? 'Due date marked not done.' : 'Due date marked done.');
      }).catch(function () { toast('Could not save the due date.'); });
    } else if (act === 'comment-order') {
      toggleCommentOrder();
    } else if (act === 'add-comment') {
      if (showModalTab('comments')) return;
      var commentInput = document.getElementById('modal-comment-input');
      if (commentInput) { commentInput.focus(); commentInput.scrollIntoView({ behavior: 'smooth', block: 'center' }); }
    } else if (act === 'add-attachment') {
      if (showModalTab('files')) return;
      var attachMenu = document.getElementById('tm-attach-menu');
      if (attachMenu) attachMenu.hidden = false;
    } else if (act === 'upload-attachment') {
      if (showModalTab('files')) return;
      var fileInput = document.getElementById('modal-attachment-file');
      if (fileInput) fileInput.click();
    } else if (act === 'new-document') {
      if (showModalTab('files')) return;
      newDocument();
    } else if (act === 'attach-link') {
      if (showModalTab('files')) return;
      openAttachmentLink();
    } else if (act === 'add-relation') {
      if (showModalTab('relations')) return;
      openRelationDialog();
    }
  }

  function copyTaskUrl(id) {
    if (!id) return;
    var url = window.location.origin + '/b/' + modalBoardId() + '#task-' + id;
    function done() { toast('Task URL copied to clipboard.'); }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(url).then(done, function () { toast(url); });
    } else {
      toast(url);
    }
  }

  var copySource = null;

  function openCopyDialog(taskId) {
    var id = taskId || modalTaskId();
    var bid = modalBoardId();
    if (!id || !bid) return;
    Promise.all([
      fetchJson('/api/tasks/' + encodeURIComponent(id)),
      fetchJson('/api/boards/' + encodeURIComponent(bid) + '/columns'),
    ]).then(function (loaded) {
      copySource = loaded[0];
      var colSel = document.getElementById('cp-col');
      colSel.innerHTML = '';
      (loaded[1] || []).forEach(function (c) {
        var o = document.createElement('option');
        o.value = c.id;
        o.textContent = c.name;
        colSel.appendChild(o);
      });
      colSel.value = copySource.column_id;
      var nameInput = document.getElementById('cp-name');
      nameInput.value = copySource.name + ' (copy)';
      document.getElementById('copy-task-dialog').hidden = false;
      nameInput.focus();
      nameInput.select();
    }).catch(function () { toast('Could not load the task.'); });
  }

  function doCopyTask() {
    var source = copySource;
    if (!source) return;
    var colSel = document.getElementById('cp-col');
    var name = document.getElementById('cp-name').value.trim();
    if (!name) { toast('Enter a name for the copy.'); return; }
    if (!colSel.value) { toast('Choose a target column.'); return; }
    var columnName = colSel.options[colSel.selectedIndex].textContent;
    var copyBtn = document.getElementById('cp-copy-btn');
    if (copyBtn.disabled) return;
    copyBtn.disabled = true;
    api('/api/tasks', 'POST', {
      column_id: colSel.value,
      swimlane_id: source.swimlane_id || null,
      name: name,
      description: source.description,
      color_id: source.color_id,
      labels: source.labels,
      timeline_start: source.timeline_start,
      timeline_end: source.timeline_end,
    }).then(function (res) {
      if (!res.ok) { return toastRefusal(res, 'Could not copy task.'); }
      return drained(res).then(function () {
        document.getElementById('copy-task-dialog').hidden = true;
        var message = 'Task was copied to column ' + columnName + '.';
        if (modalTaskId()) { modalDirty = true; toast(message); return; }
        try { window.sessionStorage.setItem('pomb.pendingToast', message); } catch (e) {}
        window.location.reload();
      });
    }).catch(function () { toast('Could not copy task.'); }).then(function () { copyBtn.disabled = false; });
  }

  // KF-070/KF-334: the "Move task" dialog for `taskId` (default: the open
  // modal's task). Given `anchorCard`, it opens as a popover at that card.
  function openMoveDialog(taskId, anchorCard, bulkIds) {
    var id = taskId || modalTaskId();
    if (!id && !bulkIds) return;
    moveTaskId = id;
    moveBulkIds = bulkIds || null;
    document.getElementById('mt-title').textContent = bulkIds ? 'Move ' + bulkIds.length + ' tasks' : 'Move task';
    var card = anchorCard || document.querySelector('.task-card[data-task-id="' + cssEscape(id) + '"]');
    var list = card ? card.closest('.task-list') : null;
    var headers = Array.prototype.slice.call(document.querySelectorAll('#column-headers-row .columnHeader'));
    var boardCols = headers.map(function (h) { return { id: h.dataset.columnId, name: h.dataset.columnName }; });
    var origColId = list ? list.dataset.columnId : (boardCols[0] ? boardCols[0].id : null);
    var origLane = (list && list.dataset.swimlaneId) ? list.dataset.swimlaneId : null;
    var origCol = boardCols.filter(function (c) { return c.id === origColId; })[0];
    var origColName = origCol ? origCol.name : '';
    var currentBid = modalBoardId();
    var boardSel = document.getElementById('mt-board');
    var colSel = document.getElementById('mt-col');

    // Lanes are per-board: keep the card's lane on this board; the server
    // picks the target board's lane on a cross-board move.
    function syncTarget() {
      moveTargetColumnId = colSel.value || null;
      moveSwimlaneId = !bulkIds && boardSel.value === currentBid ? origLane : null;
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
    here.textContent = boardNameFor(currentBid);
    boardSel.appendChild(here);
    fillColumns(boardCols, origColId);
    if (!boardCols.length && currentBid) {
      Promise.all([
        fetchJson('/api/boards/' + encodeURIComponent(currentBid) + '/columns'),
        fetchJson('/api/tasks/' + encodeURIComponent(id)),
      ]).then(function (loaded) {
        if (boardSel.value !== currentBid || moveTaskId !== id) return;
        boardCols = loaded[0] || [];
        origColId = loaded[1].column_id;
        origLane = loaded[1].swimlane_id || null;
        origCol = boardCols.filter(function (c) { return c.id === origColId; })[0];
        origColName = origCol ? origCol.name : '';
        fillColumns(boardCols, origColId);
      }).catch(function () { toast('Could not load board columns.'); });
    }
    fetchJson('/api/boards').then(function (boards) {
      boardSel.innerHTML = '';
      var listed = (boards || []).slice();
      if (currentBid && !listed.some(function (b) { return b.id === currentBid; })) {
        listed.unshift({ id: currentBid, name: here.textContent });
      }
      listed.forEach(function (b) {
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
    if (moveBulkIds && moveTargetColumnId) {
      postBulk({ task_ids: moveBulkIds, action: 'move', column_id: moveTargetColumnId, swimlane_id: null })
        .then(function (res) {
          document.getElementById('move-task-dialog').hidden = true;
          if (!res.ok) { toast('Could not move the selected tasks.'); return; }
          window.location.reload();
        });
      return;
    }
    var id = moveTaskId;
    if (!id) return;
    if (!moveTargetColumnId) { toast('Could not move task.'); return; }
    api('/api/tasks/' + encodeURIComponent(id) + '/move?force=true', 'POST',
        { column_id: moveTargetColumnId, swimlane_id: moveSwimlaneId })
      .then(drained)
      .then(function (res) {
        document.getElementById('move-task-dialog').hidden = true;
        if (!res.ok) { toastRefusal(res, 'Could not move task.'); return; }
        if (modalTaskId() === id) { modalDirty = true; closeModal(); }
        else window.location.reload();
      });
  }

  // KF-547: a 184 px popover beside the modal's Add button (golden modal/27).
  function openEstimateDialog() {
    var overlay = document.getElementById('estimate-dialog');
    var dlg = overlay.querySelector('.dlg');
    var input = document.getElementById('est-input');
    input.value = '';
    var modal = document.querySelector('.task-modal');
    var points = !!modal && modal.dataset.estimateUnit === 'points';
    var heading = points ? 'Add points estimate' : 'Add time estimate';
    overlay.dataset.unit = points ? 'points' : 'hours';
    overlay.querySelector('.dlg-title span').textContent = heading;
    dlg.setAttribute('aria-label', heading);
    input.placeholder = points ? '0 points' : '0h';
    input.title = points ? 'Estimate in points, e.g. 5' : 'Estimate, e.g. 2h, 30m, 1h 30m';
    input.setAttribute('aria-label', points ? 'Points estimate (e.g. 5)' : 'Estimate (e.g. 2h, 30m, 1h 30m)');
    overlay.hidden = false;
    var add = document.querySelector('.task-modal-iconbar [data-tm-menu="tm-add-menu"]');
    var circle = add ? add.querySelector('.tm-icon') : null;
    var a = circle ? circle.getBoundingClientRect() : { right: window.innerWidth - dlg.offsetWidth - 8, top: 49 };
    dlg.style.left = Math.max(0, Math.min(a.right + 9.5, window.innerWidth - dlg.offsetWidth)) + 'px';
    dlg.style.top = Math.max(0, Math.min(a.top + 7, window.innerHeight - dlg.offsetHeight)
) + 'px';
    input.focus();
  }

  // "Add time estimate": the estimate is stored hour-based (KanbanFlow

  // parity, KF-216); the server derives the pomodoro size from it.
  function doAddEstimate() {
    var id = modalTaskId();
    if (!id) return;
    var raw = document.getElementById('est-input').value;
    if (document.getElementById('estimate-dialog').dataset.unit === 'points') {
      var points = Math.round(parseFloat(raw) * 100) / 100;
      if (!(points > 0)) { toast('Enter the points as a number of at least 0.01, like 5.'); return; }
      api('/api/tasks/' + encodeURIComponent(id), 'PATCH', { points: points })
        .then(function (res) {
          document.getElementById('estimate-dialog').hidden = true;
          if (res.ok) { modalDirty = true; refreshModal(); }
          else toast('Could not save estimate.');
        });
      return;
    }
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

  var COMMENT_FILTER_KEY = 'pomb.hiddenCommentKinds';

  var hiddenCommentKindsFallback = [];

  function loadHiddenCommentKinds() {
    try {
      var raw = JSON.parse(window.localStorage.getItem(COMMENT_FILTER_KEY) || '[]');
      hiddenCommentKindsFallback = Array.isArray(raw)
        ? raw.filter(function (k) { return typeof k === 'string'; })
        : [];
    } catch (e) { /* storage unavailable: keep the page-session copy */ }
    return hiddenCommentKindsFallback.slice();
  }

  function saveHiddenCommentKinds(kinds) {
    hiddenCommentKindsFallback = kinds.slice();
    try { window.localStorage.setItem(COMMENT_FILTER_KEY, JSON.stringify(kinds)); } catch (e) { /* the page-session copy still holds it */ }
  }

  function wireCommentFilter() {
    var bar = document.getElementById('modal-comment-filter');
    var list = document.getElementById('modal-comments');
    if (!bar || !list) return;
    var chips = Array.prototype.slice.call(bar.querySelectorAll('[data-comment-filter]'));
    var count = document.getElementById('modal-comment-hidden');
    var hidden = loadHiddenCommentKinds();
    function apply() {
      var total = 0;
      Array.prototype.forEach.call(list.querySelectorAll('.tm-comment'), function (el) {
        var off = hidden.indexOf(el.getAttribute('data-comment-kind') || '') !== -1;
        el.hidden = off;
        if (off) total += 1;
      });
      chips.forEach(function (chip) {
        var off = hidden.indexOf(chip.getAttribute('data-comment-filter')) !== -1;
        chip.setAttribute('aria-pressed', off ? 'false' : 'true');
      });
      count.textContent = total + ' hidden';
      count.hidden = total === 0;
    }
    chips.forEach(function (chip) {
      chip.addEventListener('click', function () {
        var kind = chip.getAttribute('data-comment-filter');
        var at = hidden.indexOf(kind);
        if (at === -1) hidden.push(kind); else hidden.splice(at, 1);
        saveHiddenCommentKinds(hidden);
        apply();
      });
    });
    apply();
  }

  var COMMENT_ORDER_KEY = 'pomb.commentsNewestFirst';

  var commentsNewestFallback = false;

  function loadCommentsNewestFirst() {
    try { commentsNewestFallback = window.localStorage.getItem(COMMENT_ORDER_KEY) === '1'; } catch (e) { /* storage unavailable: keep the page-session copy */ }
    return commentsNewestFallback;
  }

  function saveCommentsNewestFirst(on) {
    commentsNewestFallback = on;
    try { window.localStorage.setItem(COMMENT_ORDER_KEY, on ? '1' : '0'); } catch (e) { /* the page-session copy still holds it */ }
  }

  function applyCommentOrder(newest) {
    var list = document.getElementById('modal-comments');
    var composer = document.querySelector('#tm-panel-comments .tm-comment-composer');
    var item = document.querySelector('[data-tm-act="comment-order"]');
    if (!list || !composer) return;
    if ((list.getAttribute('data-order') === 'newest') !== newest) {
      Array.prototype.slice.call(list.children).reverse().forEach(function (row) { list.appendChild(row); });
      list.setAttribute('data-order', newest ? 'newest' : 'oldest');
    }
    list.parentNode.insertBefore(composer, newest ? list : list.nextSibling);
    if (item) item.textContent = newest ? 'Change order - oldest to newest' : 'Change order - newest to oldest';
  }

  function toggleCommentOrder() {
    var newest = !loadCommentsNewestFirst();
    saveCommentsNewestFirst(newest);
    applyCommentOrder(newest);
  }

  var MENTION_LIST_MAX = 8;

  var mentionMembers = null;

  function loadMentionMembers() {
    if (!mentionMembers) {
      mentionMembers = api('/api/members', 'GET')
        .then(function (res) { if (!res.ok) throw new Error('members failed'); return res.json(); })
        .catch(function () { mentionMembers = null; return []; });
    }
    return mentionMembers;
  }

  function mentionAt(value, caret) {
    var m = /(^|\s)@([\w.-]*)$/.exec(value.slice(0, caret));
    return m ? { start: caret - m[2].length - 1, query: m[2].toLowerCase() } : null;
  }

  function mentionMatches(members, query) {
    return members.filter(function (member) {
      return member.username.toLowerCase().indexOf(query) !== -1
        || (member.display_name || '').toLowerCase().indexOf(query) !== -1;
    }).slice(0, MENTION_LIST_MAX);
  }

  function mentionInsert(value, caret, at, username) {
    var text = '@' + username + (/^\s/.test(value.slice(caret)) ? '' : ' ');
    return { value: value.slice(0, at.start) + text + value.slice(caret), caret: at.start + text.length };
  }

  function caretOf(input) {
    return input.selectionStart == null ? input.value.length : input.selectionStart;
  }

  function wireMentions(input) {
    var pop = null;
    var matches = [];
    var picked = 0;
    var at = null;

    function close() {
      if (pop) pop.hidden = true;
      matches = [];
      at = null;
    }

    function choose(member) {
      var caret = caretOf(input);
      var next = mentionInsert(input.value, caret, at, member.username);
      input.value = next.value;
      close();
      input.focus();
      try { input.setSelectionRange(next.caret, next.caret); } catch (ignore) {}
    }

    function draw() {
      if (!pop) {
        pop = document.createElement('div');
        pop.className = 'tm-mention-pop';
        pop.setAttribute('role', 'listbox');
        pop.setAttribute('aria-label', 'Mention a member');
        document.body.appendChild(pop);
      }
      pop.textContent = '';
      matches.forEach(function (member, i) {
        var b = document.createElement('button');
        b.type = 'button';
        b.setAttribute('role', 'option');
        b.setAttribute('data-mention', member.username);
        b.setAttribute('aria-selected', i === picked ? 'true' : 'false');
        b.textContent = '@' + member.username + (member.display_name && member.display_name !== member.username ? ' ' + member.display_name : '');
        b.addEventListener('mousedown', function (e) { e.preventDefault(); choose(member); });
        pop.appendChild(b);
      });
      var r = input.getBoundingClientRect();
      pop.style.left = r.left + 'px';
      pop.style.top = (r.bottom + 2) + 'px';
      pop.hidden = matches.length === 0;
    }

    input.addEventListener('input', function () {
      var caret = caretOf(input);
      var found = mentionAt(input.value, caret);
      if (!found) { close(); return; }
      loadMentionMembers().then(function (members) {
        if (document.activeElement !== input) return;
        var now = mentionAt(input.value, caretOf(input));
        if (!now) return;
        at = now;
        matches = mentionMatches(members, now.query);
        picked = 0;
        draw();
      });
    });
    input.addEventListener('keydown', function (e) {
      if (!pop || pop.hidden || matches.length === 0) return;
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault();
        picked = (picked + (e.key === 'ArrowDown' ? 1 : matches.length - 1)) % matches.length;
        draw();
      } else if ((e.key === 'Enter' && !e.metaKey && !e.ctrlKey) || e.key === 'Tab') {
        e.preventDefault();
        e.stopImmediatePropagation();
        choose(matches[picked]);
      } else if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        close();
      }
    });
    input.addEventListener('blur', close);
  }

  function wireModalTaskRefs() {
    Array.prototype.forEach.call(document.querySelectorAll('#modal-root [data-open-task]'), function (el) {
      var open = function () { openRelatedTask(el); };
      el.addEventListener('click', open);
      el.addEventListener('keydown', function (e) {
        if ((e.key !== 'Enter' && e.key !== ' ') || e.metaKey || e.ctrlKey || e.altKey) return;
        e.preventDefault();
        open();
      });
    });
  }

  function wireModal() {
    var overlay = document.getElementById('modal-overlay');
    if (!overlay) return;
    overlay.addEventListener('click', function (e) {
      if (e.target === overlay) closeModal();
    });
    buildModalColorPicker();
    buildTableFields();
    wireCommentFilter();
    applyCommentOrder(loadCommentsNewestFirst());
    var draftInput = document.getElementById('modal-comment-input');
    if (draftInput && pendingCommentDraft !== null) draftInput.value = pendingCommentDraft;
    pendingCommentDraft = null;
    wireModalTabs();
    wireModalTaskRefs();
    wireFieldGroups();
    wireSidebarFolds();
    var nameInput = document.getElementById('modal-name');
    if (nameInput) {
      nameInput.addEventListener('change', saveModalName);
      // KF-524: the title is a textarea that grows to its wrapped lines;
      // Enter saves instead of adding a line.
      var fitTitle = function () {
        nameInput.style.height = 'auto';
        nameInput.style.height = nameInput.scrollHeight + 2 + 'px';
      };
      nameInput.addEventListener('input', function () {
        var flat = nameInput.value.replace(/[\r\n]+/g, ' ');
        if (flat !== nameInput.value) nameInput.value = flat;
        fitTitle();
      });
      nameInput.addEventListener('keydown', function (e) {
        if (e.key === 'Enter' && !e.ctrlKey && !e.metaKey && !e.shiftKey) {
          e.preventDefault();
          nameInput.blur();
        }
      });
      fitTitle();
      if (window.ResizeObserver) new ResizeObserver(fitTitle).observe(nameInput);
    }
    var descBlock = document.getElementById('modal-description-text');
    if (descBlock) {
      bindDescriptionBlock(descBlock);
    }
    syncMdToggle();
    // KF-254: the composer submits on Enter (no Add button), Shift+Enter adds a line.
    var commentInput = document.getElementById('modal-comment-input');
    if (commentInput) {
      wireMentions(commentInput);
      bindImagePaste(commentInput);
      commentInput.addEventListener('keydown', onCommentKeydown);
      commentInput.addEventListener('input', () => fitCommentBox(commentInput));
      fitCommentBox(commentInput);
    }
    var closeBtn = overlay.querySelector('[data-close-modal]');
    if (closeBtn) closeBtn.addEventListener('click', closeModal);
    wireAttachments(overlay);
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
        modalAction(btn.getAttribute('data-tm-act'), btn);
      });
    });
    wireSubtasks();
    wireChildren(overlay);
    wireRelations(overlay);
    wireOwnerPanel(overlay);
    wireMetadata(overlay);

    // KF-278: Time spent hover menu with tooltip and action menu
    wireTimeSpentHover();
    wireModalScrollShadow(overlay);
  }

  function wireModalScrollShadow(overlay) {
    var modal = overlay.querySelector('.task-modal');
    if (!modal) return;
    overlay.addEventListener('scroll', function () {
      var scrolled = Array.prototype.some.call(modal.querySelectorAll('.tm-main, .tm-side, .tm-layout, .tm-subview'), function (el) {
        return el.scrollTop > 0;
      });
      modal.classList.toggle('tm-scrolled', scrolled);
    }, true);
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
      var tabCount = document.getElementById('modal-subtask-tabcount');
      if (tabCount) tabCount.textContent = total;
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
        '<ellipse cx="3" cy="2.5" rx="2.0" ry="2.25" fill="currentColor"/>' +
        '<ellipse cx="3" cy="8" rx="2.0" ry="2.25" fill="currentColor"/>' +
        '<ellipse cx="3" cy="13.5" rx="2.0" ry="2.25" fill="currentColor"/></svg>';
      div.appendChild(check);
      var members = document.createElement('span');
      members.className = 'tm-subtask-members';
      div.dataset.memberIds = '';
      div.appendChild(name);
      div.appendChild(members);
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

    function openSubtaskDialog(title, label) {
      var overlay = document.createElement('div');
      overlay.className = 'dlg-overlay dlg-overlay-top';
      var dlg = document.createElement('div');
      dlg.className = 'dlg dlg-light';
      dlg.setAttribute('role', 'dialog');
      dlg.setAttribute('aria-label', label);
      var heading = document.createElement('div');
      heading.className = 'dlg-title';
      heading.textContent = title;
      var body = document.createElement('div');
      var actions = document.createElement('div');
      actions.className = 'dlg-actions';
      var cancel = document.createElement('button');
      cancel.type = 'button';
      cancel.className = 'btn';
      cancel.textContent = 'Cancel';
      actions.appendChild(cancel);
      dlg.appendChild(heading);
      dlg.appendChild(body);
      dlg.appendChild(actions);
      overlay.appendChild(dlg);
      function close() { overlay.remove(); }
      cancel.addEventListener('click', close);
      overlay.addEventListener('click', function (e) { if (e.target === overlay) close(); });
      overlay.addEventListener('keydown', function (e) {
        if (e.key === 'Escape') { e.stopPropagation(); close(); }
      });
      document.body.appendChild(overlay);
      return { body: body, actions: actions, cancel: cancel, close: close };
    }

    function saveSubtaskTemplate() {
      var dialog = openSubtaskDialog('Save as template', 'Save subtasks as template');
      var field = document.createElement('label');
      field.className = 'dlg-field';
      var caption = document.createElement('span');
      caption.textContent = 'Name';
      var nameInput = document.createElement('input');
      nameInput.type = 'text';
      nameInput.maxLength = 200;
      nameInput.autocomplete = 'off';
      field.appendChild(caption);
      field.appendChild(nameInput);
      dialog.body.appendChild(field);
      var save = document.createElement('button');
      save.type = 'button';
      save.className = 'btn btn-primary';
      save.textContent = 'Save';
      dialog.actions.appendChild(save);
      function submit() {
        var name = nameInput.value.trim();
        if (!name) { nameInput.focus(); return; }
        api('/api/tasks/' + encodeURIComponent(taskId) + '/subtasks/template', 'POST', { name: name })
          .then(function (res) {
            if (!res.ok) { toast('Could not save the template.'); return; }
            dialog.close();
            toast('Saved template');
          })
          .catch(function () { toast('Could not save the template.'); });
      }
      save.addEventListener('click', submit);
      nameInput.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') { e.preventDefault(); submit(); }
      });
      nameInput.focus();
    }

    function addFromSubtaskTemplate() {
      var dialog = openSubtaskDialog('Add from template', 'Add subtasks from template');
      var list = document.createElement('div');
      list.className = 'tm-template-list';
      dialog.body.appendChild(list);
      var templates = [];

      function showMessage(text) {
        list.textContent = '';
        var p = document.createElement('p');
        p.className = 'dlg-text';
        p.textContent = text;
        list.appendChild(p);
      }

      function apply(template) {
        api('/api/tasks/' + encodeURIComponent(taskId) + '/subtasks/from-template', 'POST',
            { template_id: template.id })
          .then(function (res) { return res.ok ? res.json() : null; })
          .then(function (subs) {
            if (!subs) { toast('Could not add subtasks from the template.'); return; }
            subs.forEach(function (sub) { wrap.appendChild(subtaskRow(sub)); });
            updateSubtaskCount();
            modalDirty = true;
            dialog.close();
          })
          .catch(function () { toast('Could not add subtasks from the template.'); });
      }

      function remove(doomed) {
        fetch('/api/settings')
          .then(function (res) { return res.ok ? res.json() : null; })
          .then(function (fresh) {
            if (!fresh) return null;
            var rest = (fresh.subtask_templates || []).filter(function (template) { return template.id !== doomed.id; });
            return api('/api/settings', 'PUT', { subtask_templates: rest });
          })
          .then(function (res) { return res && res.ok ? res.json() : null; })
          .then(function (settings) {
            if (!settings) { toast('Could not delete the template.'); return; }
            templates = settings.subtask_templates || [];
            render();
          })
          .catch(function () { toast('Could not delete the template.'); });
      }

      function render() {
        if (!templates.length) { showMessage('No saved templates yet.'); return; }
        list.textContent = '';
        templates.forEach(function (template) {
          var row = document.createElement('div');
          row.className = 'tm-template-row';
          var pick = document.createElement('button');
          pick.type = 'button';
          pick.className = 'tm-template-pick';
          var count = template.subtasks.length;
          pick.textContent = template.name + ' (' + count + (count === 1 ? ' subtask)' : ' subtasks)');
          pick.addEventListener('click', function () { apply(template); });
          var del = document.createElement('button');
          del.type = 'button';
          del.className = 'btn btn-danger tm-template-delete';
          del.textContent = 'Delete';
          del.setAttribute('aria-label', 'Delete template ' + template.name);
          del.addEventListener('click', function () { remove(template); });
          row.appendChild(pick);
          row.appendChild(del);
          list.appendChild(row);
        });
      }

      showMessage('Loading...');
      fetch('/api/settings')
        .then(function (res) { return res.ok ? res.json() : null; })
        .then(function (settings) {
          if (!settings) { showMessage('Could not load the templates.'); return; }
          templates = settings.subtask_templates || [];
          render();
        })
        .catch(function () { showMessage('Could not load the templates.'); });
    }

    function deleteAllSubtasks() {
      showConfirmDialog('Delete all subtasks', 'Delete every subtask of this task?', 'Delete', function () {
        api('/api/tasks/' + encodeURIComponent(taskId) + '/subtasks', 'DELETE')
          .then(function (res) {
            if (!res.ok) { toast('Could not delete the subtasks.'); return; }
            wrap.querySelectorAll('.tm-subtask').forEach(function (row) { row.remove(); });
            updateSubtaskCount();
            modalDirty = true;
          })
          .catch(function () { toast('Could not delete the subtasks.'); });
      });
    }

    var memberRoster = null;
    function loadMemberRoster() {
      if (memberRoster) return Promise.resolve(memberRoster);
      return fetchJson('/api/members').then(function (members) {
        memberRoster = members || [];
        return memberRoster;
      });
    }

    function rowMemberIds(row) {
      return (row.dataset.memberIds || '').split(',').filter(Boolean);
    }

    function setRowMembers(row, ids) {
      row.dataset.memberIds = ids.join(',');
      var holder = row.querySelector('.tm-subtask-members');
      if (!holder) {
        holder = document.createElement('span');
        holder.className = 'tm-subtask-members';
        row.insertBefore(holder, row.querySelector('.tm-subtask-handle'));
      }
      holder.textContent = '';
      (memberRoster || []).forEach(function (member) {
        if (ids.indexOf(member.id) === -1) return;
        var chip = document.createElement('span');
        chip.className = 'tm-member-disc tm-subtask-member';
        chip.title = member.display_name;
        paintAvatar(chip, member);
        holder.appendChild(chip);
      });
    }

    function memberPicker(body, selected, onPick) {
      var search = document.createElement('input');
      search.type = 'text';
      search.placeholder = 'Search...';
      search.autocomplete = 'off';
      search.setAttribute('aria-label', 'Search members');
      var list = document.createElement('div');
      list.className = 'members-list';
      list.setAttribute('role', 'listbox');
      list.setAttribute('aria-label', 'Members');
      body.appendChild(search);
      body.appendChild(list);
      function render() {
        var query = search.value.trim().toLowerCase();
        var shown = 0;
        list.textContent = '';
        (memberRoster || []).forEach(function (member) {
          if (query && member.display_name.toLowerCase().indexOf(query) === -1) return;
          shown++;
          var on = selected.indexOf(member.id) !== -1;
          var item = document.createElement('button');
          item.type = 'button';
          item.className = 'member-row';
          item.setAttribute('role', 'option');
          item.setAttribute('aria-selected', on ? 'true' : 'false');
          var name = document.createElement('span');
          name.className = 'member-name';
          name.textContent = member.display_name;
          item.appendChild(name);
          if (on) {
            var check = document.createElement('span');
            check.className = 'member-check';
            check.setAttribute('aria-hidden', 'true');
            check.innerHTML = '&#10003;';
            item.appendChild(check);
          }
          item.addEventListener('click', function () { onPick(member.id); });
          list.appendChild(item);
        });
        if (!shown) {
          var empty = document.createElement('div');
          empty.className = 'member-row';
          empty.textContent = memberRoster && memberRoster.length ? 'No members match.' : 'No members.';
          list.appendChild(empty);
        }
      }
      search.addEventListener('input', render);
      loadMemberRoster().then(render, function () { toast('Could not load the members.'); });
      return { search: search, render: render };
    }

    function assignSubtask(row) {
      var selected = rowMemberIds(row);
      var dialog = openSubtaskDialog('Assign', 'Assign subtask');
      dialog.cancel.textContent = 'Done';
      var picker = memberPicker(dialog.body, selected, function (id) {
        var next = selected.slice();
        var at = next.indexOf(id);
        if (at === -1) next.push(id);
        else next.splice(at, 1);
        api('/api/tasks/' + encodeURIComponent(taskId) + '/subtasks/' +
            encodeURIComponent(row.dataset.subtaskId), 'PATCH', { member_ids: next })
          .then(function (res) {
            if (!res.ok) { toast('Could not assign the subtask.'); return; }
            selected.length = 0;
            next.forEach(function (kept) { selected.push(kept); });
            setRowMembers(row, selected);
            picker.render();
            modalDirty = true;
          })
          .catch(function () { toast('Could not assign the subtask.'); });
      });
      picker.search.focus();
    }

    function dialogSelect(dialog, caption, options, value) {
      var field = document.createElement('label');
      field.className = 'dlg-field';
      var text = document.createElement('span');
      text.textContent = caption;
      var select = document.createElement('select');
      options.forEach(function (option) {
        var el = document.createElement('option');
        el.value = option[0];
        el.textContent = option[1];
        select.appendChild(el);
      });
      select.value = value;
      field.appendChild(text);
      field.appendChild(select);
      dialog.body.appendChild(field);
      return select;
    }

    function convertSubtask(row) {
      var boardId = modalBoardId();
      if (!boardId) { toast('Could not convert the subtask.'); return; }
      var board = encodeURIComponent(boardId);
      Promise.all([
        fetchJson('/api/boards/' + board + '/colors'),
        fetchJson('/api/boards/' + board + '/columns'),
        fetchJson('/api/tasks/' + encodeURIComponent(taskId)),
        loadMemberRoster(),
      ]).then(function (loaded) {
        var colors = (loaded[0].colors || loaded[0]).filter(function (c) { return c.enabled; });
        var dialog = openSubtaskDialog('Convert to task', 'Convert subtask to task');
        var colorOptions = [['', 'Board default']].concat(colors.map(function (c) {
          return [c.id, c.label || c.standard_name];
        }));
        var color = dialogSelect(dialog, 'Colour', colorOptions, '');
        var chosen = [];
        var members = document.createElement('div');
        members.className = 'dlg-field';
        var membersCaption = document.createElement('span');
        membersCaption.textContent = 'Members';
        members.appendChild(membersCaption);
        dialog.body.appendChild(members);
        var picker = memberPicker(members, chosen, function (id) {
          var at = chosen.indexOf(id);
          if (at === -1) chosen.push(id);
          else chosen.splice(at, 1);
          picker.render();
        });
        var labelField = document.createElement('label');
        labelField.className = 'dlg-field';
        var labelCaption = document.createElement('span');
        labelCaption.textContent = 'Labels';
        var labelInput = document.createElement('input');
        labelInput.type = 'text';
        labelInput.placeholder = 'Comma separated';
        labelInput.autocomplete = 'off';
        labelField.appendChild(labelCaption);
        labelField.appendChild(labelInput);
        dialog.body.appendChild(labelField);
        var column = dialogSelect(dialog, 'Target column', loaded[1].map(function (c) {
          return [c.id, c.name];
        }), loaded[2].column_id);
        var relation = dialogSelect(dialog, 'Relation', [
          ['none', 'None'],
          ['related', 'Related to parent'],
          ['parent_depends_on_new', 'Parent depends on new task'],
          ['parent_required_by_new', 'Parent required by new task'],
        ], 'none');
        var convert = document.createElement('button');
        convert.type = 'button';
        convert.className = 'btn btn-primary';
        convert.textContent = 'Convert';
        dialog.actions.appendChild(convert);
        convert.addEventListener('click', function () {
          convert.disabled = true;
          var body = {
            color_id: color.value || null,
            member_ids: chosen,
            labels: labelInput.value.split(',').map(function (l) { return l.trim(); }).filter(Boolean),
            column_id: column.value,
            relation: relation.value,
          };
          api('/api/tasks/' + encodeURIComponent(taskId) + '/subtasks/' +
              encodeURIComponent(row.dataset.subtaskId) + '/convert', 'POST', body)
            .then(function (res) {
              if (!res.ok) { convert.disabled = false; return toastRefusal(res, 'Could not convert the subtask.'); }
              row.remove();
              updateSubtaskCount();
              modalDirty = true;
              dialog.close();
              toast('Subtask converted to a task.');
            })
            .catch(function () { convert.disabled = false; toast('Could not convert the subtask.'); });
        });
      }).catch(function () { toast('Could not convert the subtask.'); });
    }

    var subtaskActions = {
      'save-template': saveSubtaskTemplate,
      'add-from-template': addFromSubtaskTemplate,
      'delete-all': deleteAllSubtasks,
    };
    document.querySelectorAll('[data-tm-subtask-act]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        closeAllTmMenus();
        subtaskActions[btn.getAttribute('data-tm-subtask-act')]();
      });
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
        var dueItem = document.createElement('button');
        dueItem.type = 'button';
        dueItem.setAttribute('role', 'menuitem');
        dueItem.className = 'tm-subtask-due-item';
        dueItem.addEventListener('click', function () {
          var row = subtaskMenu._row;
          closeAllTmMenus();
          if (!row || !row.isConnected) return;
          DueDateDialog.open(row.querySelector('.tm-subtask-handle'), row.dataset.subtaskId);
        });
        subtaskMenu.appendChild(dueItem);
        [['Assign', assignSubtask], ['Convert to task', convertSubtask]].forEach(function (entry) {
          var action = document.createElement('button');
          action.type = 'button';
          action.textContent = entry[0];
          action.setAttribute('role', 'menuitem');
          action.addEventListener('click', function () {
            var row = subtaskMenu._row;
            closeAllTmMenus();
            if (!row || !row.isConnected) return;
            entry[1](row);
          });
          subtaskMenu.appendChild(action);
        });
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
          menu.querySelector('.tm-subtask-due-item').textContent =
            menuRow.querySelector('.tm-subtask-due') ? 'Edit due date' : 'Add due date';
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
      var edit = document.createElement('textarea');
      edit.rows = 1;
      edit.className = 'tm-subtask-edit';
      edit.value = current;
      edit.maxLength = 200;
      edit.setAttribute('aria-label', 'Edit subtask');
      const fit = () => {
        edit.style.height = 'auto';
        edit.style.height = edit.scrollHeight + 'px';
      };
      nameEl.replaceWith(edit);
      fit();
      edit.focus();
      edit.select();
      edit.addEventListener('input', fit);
      var settled = false;
      function finish(save) {
        if (settled) return;
        settled = true;
        var val = edit.value.replace(/\s*\n\s*/g, ' ').trim();
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
  var membersBulkIds = null;
  var membersRoster = [];
  var membersAssigned = [];
  var membersResponsible = null;

  function closeMemberMenu() {
    var menu = document.getElementById('member-menu');
    if (!menu || menu.hidden) return;
    menu.hidden = true;
    var open = document.querySelector('.member-menu-btn[aria-expanded="true"]');
    if (open) open.setAttribute('aria-expanded', 'false');
  }

  function openMemberMenu(button) {
    var dlg = document.getElementById('members-dialog');
    var menu = document.getElementById('member-menu');
    if (!menu) {
      menu = document.createElement('div');
      menu.id = 'member-menu';
      menu.className = 'menu-pop menu-floating';
      menu.setAttribute('role', 'menu');
      menu.setAttribute('aria-label', 'Member menu');
      var item = document.createElement('button');
      item.type = 'button';
      item.setAttribute('role', 'menuitem');
      item.textContent = 'Set as responsible';
      item.addEventListener('click', function () {
        var memberId = menu.dataset.memberId;
        closeMemberMenu();
        setResponsibleMember(memberId);
      });
      menu.appendChild(item);
      dlg.appendChild(menu);
    }
    var wasOpen = !menu.hidden && button.getAttribute('aria-expanded') === 'true';
    closeMemberMenu();
    if (wasOpen) return;
    menu.dataset.memberId = button.dataset.memberId;
    var rect = button.getBoundingClientRect();
    menu.style.top = (rect.bottom + 4) + 'px';
    menu.style.left = Math.max(8, rect.right - 184) + 'px';
    menu.hidden = false;
    button.setAttribute('aria-expanded', 'true');
    menu.querySelector('button').focus();
  }

  function setResponsibleMember(memberId) {
    if (!membersDialogTaskId) return;
    var taskId = membersDialogTaskId;
    api('/api/tasks/' + encodeURIComponent(taskId), 'PATCH', { responsible: { kind: 'member', id: memberId } })
      .then(function (res) {
        if (!res.ok) { toast('Could not set the responsible member.'); return; }
        membersResponsible = { kind: 'member', id: memberId };
        refreshAfterMemberChange(taskId);
      });
  }

  function refreshAfterMemberChange(taskId) {
    renderMembersList(document.getElementById('members-search').value);
    // A card on the board re-renders so its avatars and its
    // place under an active filter follow the new members.
    if (typeof TimerUI !== 'undefined' && TimerUI.refreshTaskCard) TimerUI.refreshTaskCard(taskId);
    // Refresh the modal body row when the dialog was opened for the
    // modal's task.
    if (modalTaskId() && modalTaskId() === taskId) {
      modalDirty = true;
      openModal(taskId);
      // Keep the dialog on top after the modal re-renders.
      document.getElementById('members-dialog').hidden = false;
      document.getElementById('members-search').focus();
    }
  }

  function initMembersDialog() {
    var dlg = document.getElementById('members-dialog');
    if (!dlg || dlg._wired) return;
    dlg._wired = true;
    document.getElementById('members-search').addEventListener('input', function (e) {
      renderMembersList(e.target.value);
    });
    document.getElementById('members-list').addEventListener('click', function (e) {
      var menuBtn = e.target.closest('.member-menu-btn');
      if (menuBtn) { openMemberMenu(menuBtn); return; }
      closeMemberMenu();
      var row = e.target.closest('.member-row[data-member-id]');
      if (row) toggleMemberAssignment(row.dataset.memberId);
    });
    dlg.addEventListener('click', function (e) {
      if (!e.target.closest('#member-menu') && !e.target.closest('.member-menu-btn')) closeMemberMenu();
    });
    dlg.addEventListener('keydown', function (e) {
      var menu = document.getElementById('member-menu');
      if (e.key === 'Escape' && menu && !menu.hidden) {
        e.stopPropagation();
        closeMemberMenu();
      }
    });
    document.getElementById('members-gear').addEventListener('click', function () {
      window.location.href = '/settings';
    });
  }

  function openMembersDialog(taskId) {
    var anchor = taskAnchor(taskId, '#modal-members-row .tm-member-add');
    initMembersDialog();
    membersBulkIds = null;
    membersDialogTaskId = taskId || null;
    membersRoster = [];
    membersAssigned = [];
    membersResponsible = null;
    var search = document.getElementById('members-search');
    var list = document.getElementById('members-list');
    search.value = '';
    list.innerHTML = '<div class="member-row">Loading…</div>';
    anchorPopover(document.getElementById('members-dialog'), anchor);
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
      membersResponsible = (results[1] && results[1].responsible) || null;
      renderMembersList('');
    });
  }

  function renderMembersList(filter) {
    closeMemberMenu();
    var list = document.getElementById('members-list');
    list.innerHTML = '';
    var q = (filter || '').trim().toLowerCase();
    var shown = 0;
    membersRoster.forEach(function (m) {
      if (q && m.display_name.toLowerCase().indexOf(q) === -1) return;
      shown++;
      var row = document.createElement('button');
      row.type = 'button';
      row.className = 'member-row';
      row.setAttribute('role', 'option');
      var isAssigned = membersAssigned.indexOf(m.id) !== -1;
      row.setAttribute('aria-selected', isAssigned ? 'true' : 'false');
      row.dataset.memberId = m.id;
      var box = document.createElement('input');
      box.type = 'checkbox';
      box.className = 'member-box';
      box.checked = isAssigned;
      box.tabIndex = -1;
      box.setAttribute('aria-hidden', 'true');
      var avatar = document.createElement('span');
      avatar.className = 'member-avatar';
      avatar.setAttribute('aria-hidden', 'true');
      paintAvatar(avatar, m);
      var name = document.createElement('span');
      name.className = 'member-name';
      name.textContent = m.display_name;
      row.appendChild(box);
      row.appendChild(avatar);
      row.appendChild(name);
      var line = document.createElement('div');
      line.className = 'member-line';
      line.appendChild(row);
      if (isAssigned && !membersBulkIds) {
        var isResponsible = !!membersResponsible &&
          membersResponsible.kind === 'member' && membersResponsible.id === m.id;
        if (isResponsible) {
          var tag = document.createElement('span');
          tag.className = 'member-responsible-tag';
          tag.textContent = 'Responsible';
          row.appendChild(tag);
        }
        var menuBtn = document.createElement('button');
        menuBtn.type = 'button';
        menuBtn.className = 'member-menu-btn';
        menuBtn.dataset.memberId = m.id;
        menuBtn.setAttribute('aria-haspopup', 'menu');
        menuBtn.setAttribute('aria-expanded', 'false');
        menuBtn.setAttribute('aria-label', 'Menu for ' + m.display_name);
        menuBtn.innerHTML = '&#8943;';
        line.appendChild(menuBtn);
      }
      list.appendChild(line);
    });
    if (!shown) {
      var empty = document.createElement('div');
      empty.className = 'member-row';
      empty.textContent = membersRoster.length ? 'No members match.' : 'No members on this board.';
      list.appendChild(empty);
    }
  }

  function toggleMemberAssignment(memberId) {
    if (membersBulkIds) { toggleBulkMember(memberId); return; }
    if (!membersDialogTaskId) { toast('Open a task to assign members.'); return; }
    var next = membersAssigned.slice();
    var idx = next.indexOf(memberId);
    if (idx === -1) next.push(memberId);
    else next.splice(idx, 1);
    api('/api/tasks/' + encodeURIComponent(membersDialogTaskId), 'PATCH', { member_ids: next })
      .then(function (res) {
        if (!res.ok) { toast('Could not update members.'); return; }
        membersAssigned = next;
        return fetchJson('/api/tasks/' + encodeURIComponent(membersDialogTaskId))
          .then(function (task) { membersResponsible = task.responsible || null; },
                function () {})
          .then(function () { refreshAfterMemberChange(membersDialogTaskId); });
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
          '<span class="ts-drag" title="Drag to reorder" aria-label="Drag to reorder"><svg class="ico-grip" width="10" height="16" viewBox="0 0 10 16" fill="currentColor" aria-hidden="true" focusable="false"><circle cx="2.5" cy="2.5" r="1.5"/><circle cx="7.5" cy="2.5" r="1.5"/><circle cx="2.5" cy="8" r="1.5"/><circle cx="7.5" cy="8" r="1.5"/><circle cx="2.5" cy="13.5" r="1.5"/><circle cx="7.5" cy="13.5" r="1.5"/></svg></span>' +
          '<span class="ts-reason-label" data-index="' + i + '" title="Click to rename">' +
          escapeHtml(reasons[i]) + '</span>' +
          '<button type="button" class="ts-reason-remove" aria-label="Remove">&times;</button></div>';
      }
      list.innerHTML = html || '<p class="settings-hint">No reasons yet.</p>';
      var self = this;
      list.querySelectorAll('.ts-reason-label').forEach(function (label) {
        label.addEventListener('click', function () { self.renameReason(label); });
      });
      list.querySelectorAll('.ts-reason-remove').forEach(function (btn, index) {
        btn.addEventListener('click', function () { self.removeReason(index); });
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
          },
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
        body: JSON.stringify({ interrupt_reasons: self.settings.interrupt_reasons || [] }),
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
        body: JSON.stringify({ break_activities: acts }),
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
        body: JSON.stringify({ break_activities: acts }),
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
          '<button type="button" class="ts-activity-remove" aria-label="Delete">&times;</button></div>';
      }
      list.innerHTML = html;
      var self = this;
      list.querySelectorAll('.ts-activity-remove').forEach(function (btn, index) {
        btn.addEventListener('click', function () { self.deleteActivity(index); });
      });
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
        interrupt_reasons: this.settings ? this.settings.interrupt_reasons : undefined,
      };
      fetch('/api/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify(body),
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
    },
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

  function toastRefusal(res, title) {
    return res.text().then(function (text) {
      var message = text;
      try {
        message = JSON.parse(text).message || text;
      } catch (e) { /* a plain-text body */ }
      toast(title, message);
      return null;
    });
  }

  function timerJson(res, title) {
    return res.ok ? jsonIfJson(res) : toastRefusal(res, title);
  }

  function fetchAllTasks() {
    var all = [];
    function page(cursor) {
      var url = '/api/tasks?limit=1000' + (cursor ? '&cursor=' + encodeURIComponent(cursor) : '');
      return fetch(url, { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then(function (res) { return jsonIfJson(res); })
        .then(function (body) {
          if (!body || !body.items) return null;
          all = all.concat(body.items);
          return body.next_cursor ? page(body.next_cursor) : all;
        });
    }
    return page(null);
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
      this.watchOtherTabs();
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
      return fetch('/api/timer/status', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
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
      // KF-006: a session already at 00:00 when this page first sees it
      // (a reload) does not chime again.
      var prevStart = this.state ? this.state.startedAt : null;
      if (status.mode && status.mode !== 'stopwatch' && status.remaining_seconds === 0 &&
          status.started_at !== prevStart) {
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
      // KF-465: a new break opens on its own (Pomodoro) tab.
      var breakNow = status.mode === 'short_break' || status.mode === 'long_break';
      if (breakNow && status.started_at !== prevStart && this.currentModeTab !== 'pomodoro') {
        this.setModeTab('pomodoro');
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
        var raw = window.localStorage.getItem('pomb.timerTask');
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
      return fetchAllTasks()
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
          window.localStorage.setItem('pomb.timerTask', JSON.stringify(this.selectedTask));
        } else {
          window.localStorage.removeItem('pomb.timerTask');
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

    // KF-451: the elapsed time a stop would log, as the server caps it: an
    // hour past a timed session's duration, 12 hours for a stopwatch.
    loggableElapsed: function () {
      var s = this.state;
      var cap = s && s.totalSeconds ? s.totalSeconds + 3600 : 12 * 3600;
      return Math.min(this.elapsed(), cap);
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
      document.querySelectorAll('.task-card').forEach(function (card) {
        var selected = !!activeId && String(card.dataset.taskId) === activeId;
        card.classList.toggle('card-timer-selected', selected);
        var timed = !!timedId && String(card.dataset.taskId) === timedId;
        var added = card.querySelector('.card-avatar-timer');
        if (timed && !card.querySelector('.card-avatar')) {
          added = document.createElement('span');
          added.className = 'card-avatar card-avatar-timer';
          if (header) {
            added.innerHTML = header.innerHTML;
            added.style.cssText = header.style.cssText;
          }
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
      var minutes = activeId ? Math.round(this.loggableElapsed() / 60) : 0;
      var self = this;
      document.querySelectorAll('.task-card').forEach(function (card) {
        if (!activeId || String(card.dataset.taskId) !== activeId) {
          self.dropCardLive(card);
          return;
        }
        var live = card.querySelector('.card-live-inc');
        if (!live) live = self.addCardLive(card, minutes);
        if (live) live.textContent = ' + ' + fmtDuration(minutes);
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
      } else if (s.mode === 'short_break' || s.mode === 'long_break') {
        // KF-426: a break shows a solid green ▶ (golden 28), counting down.
        glyph = '';
        cls = 'timer-pill-icon timer-pill-playbox';
        label = this.fmt(this.remaining());
      } else {
        // KF-325: a running session shows KanbanFlow's solid red square.
        glyph = '';
        cls = 'timer-pill-icon timer-pill-stopbox';
        label = this.fmt(s.mode === 'stopwatch' ? this.elapsed() : this.remaining());
      }
      // KF-020: the tab title counts down/up with the live timer
      // ("00:00 General — Pomb", KanbanFlow parity). KF-375: the title
      // keeps ticking while the popup hides the pill.
      if (!this._baseTitle) this._baseTitle = document.title;
      // An idle timer with a selected task shows its duration too (ref 03).
      // KF-427: while a pomodoro waits for its break the title alternates
      // "00:00" and "--------" (golden 26).
      var titleLabel = this.waitingForBreak() && !this.blinkOn() ? '--------' : label;
      document.title = active || this.selectedTask ? titleLabel + ' ' + this._baseTitle : this._baseTitle;
      // KF-239: idle shows the header clock icon, a session shows the chip.
      pill.classList.toggle('timer-pill--idle', !active);
      if (icon) { icon.innerHTML = glyph; icon.className = cls; }
      if (timeEl) timeEl.textContent = label;
      // KF-027: the running state must be visibly distinct from idle.
      // KF-426: a break's chip is not in the running style (golden 28).
      pill.classList.toggle('running', active && cls.indexOf('stopbox') !== -1);
      this.syncBell();
    },

    // KF-552: the bell carries a badge while a finished pomodoro waits for
    // its break (golden timer/26).
    syncBell: function () {
      var badge = document.querySelector('#notifications-btn .notification-badge');
      if (!badge) return;
      badge.hidden = Notifications.unread <= 0 && !this.waitingForBreak();
      badge.textContent = Notifications.badgeText();
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
      var active = !this.panelShowsIdle();
      var mode = active ? s.mode : this.currentModeTab;
      var isBreak = mode === 'short_break' || mode === 'long_break';
      var titleEl = document.getElementById('timer-popup-title');
      var labelEl = document.getElementById('timer-popup-label');
      // KF-424: a break keeps the "Pomodoro" title and reads "Break time"
      // (golden 27).
      if (titleEl) titleEl.textContent = mode === 'stopwatch' ? 'Stopwatch' : 'Pomodoro';
      if (labelEl) {
        labelEl.textContent = mode === 'stopwatch' ? 'Session time' : isBreak ? 'Break time' : 'Time until break';
      }
      var clock;
      var button;
      if (this.waitingForBreak()) {
        // KF-006: a finished pomodoro keeps running (its time still counts)
        // until Take break. KF-427: its "00:00" blinks beside the green
        // button (golden 26).
        clock = '00:00';
        button = '<button type="button" class="timer-btn timer-btn-green" id="tp-take-break">Take break</button>';
      } else if (active && isBreak) {
        // KF-424: a break offers the green ▶ Start, which skips to the next
        // pomodoro.
        clock = this.fmt(this.remaining());
        button = '<button type="button" class="timer-btn" id="tp-break-start">' +
          '<span class="timer-btn-play" aria-hidden="true"></span>Start</button>';
      } else if (active) {
        clock = this.fmt(mode === 'stopwatch' ? this.elapsed() : this.remaining());
        button = '<button type="button" class="timer-btn" id="tp-stop">' +
          '<span class="timer-btn-stop" aria-hidden="true"></span>Stop</button>';
      } else {
        clock = mode === 'stopwatch' ? '00:00' : this.fmt(this.pomodoroSeconds());
        button = '<button type="button" class="timer-btn" id="tp-start">' +
          '<span class="timer-btn-play" aria-hidden="true"></span>Start</button>';
      }
      var html = '<div class="timer-clock-row"><span class="timer-clock"' +
        (this.waitingForBreak() && !this.blinkOn() ? ' style="visibility:hidden"' : '') + '>' +
        clock + '</span>' + button + '</div>';
      var task = this.currentTask();
      if (task) {
        html += '<div class="timer-task-row">' +
          '<span class="timer-task-dot" style="background:' + escapeHtml(task.color || '#9e9e9e') + '"></span>' +
          '<span class="timer-task-name">' + escapeHtml(task.name || '') + '</span>' +
          (active && !isBreak ? '' : '<button type="button" class="timer-task-clear" id="tp-clear-task" title="Clear task" aria-label="Clear task">&times;</button>') +
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
      body.innerHTML = html;
      var on = function (id, fn) {
        var el = document.getElementById(id);
        if (el) el.addEventListener('click', fn);
      };
      on('tp-start', function () { self.startClicked(); });
      on('tp-stop', function () { self.stopClicked(); });
      on('tp-take-break', function () { self.takeBreak(); });
      on('tp-break-start', function () { self.skipBreak(); });
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
          when: LocalTime.clockOf(e.started_at, e.started_display) + '\u00a0 \u2013 \u00a0' +
            LocalTime.clockOf(e.ended_at, e.ended_display),
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
      var live = s && s.phase !== 'idle' && s.mode === 'pomodoro' ? this.loggableElapsed() : 0;
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
        if (s.remainingSeconds <= 0) this.sessionReachedZero();
      }
      this.renderPill();
      this.updatePip();
      this.drawTodayHeading();
      var clockEl = document.querySelector('#timer-popup .timer-clock');
      if (clockEl && this.waitingForBreak()) {
        clockEl.style.visibility = this.blinkOn() ? '' : 'hidden';
      } else if (clockEl && !this.panelShowsIdle()) {
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

    runKey: function () {
      var body = document.getElementById('timer-popup-body');
      if (!body) return false;
      this.renderPopup();
      var button = body.querySelector('#tp-stop, #tp-take-break, #tp-break-start, #tp-start');
      if (!button) return false;
      button.click();
      return true;
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
    // below its top), right edge 6 px left of the filter button.
    positionPopup: function () {
      var popup = document.getElementById('timer-popup');
      if (!popup) return;
      var w = popup.offsetWidth || 260;
      var funnel = document.getElementById('filter-btn');
      var row = document.querySelector('.board-header-row');
      var left;
      var top;
      if (funnel && row) {
        left = funnel.getBoundingClientRect().left - 6 - w;
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
        var timedCard = document.querySelector('.task-card[data-task-id="' + cssEscape(taskId) + '"]');
        showConfirmDialog('Delete task', deleteConfirmMessage(taskName, timedCard), 'Delete', function () {
          api('/api/tasks/' + encodeURIComponent(taskId), 'DELETE')
            .then(drained)
            .then(function (res) {
              if (!res.ok) { toast('Could not delete task.'); return; }
              var card = document.querySelector('.task-card[data-task-id="' + cssEscape(taskId) + '"]');
              var list = card ? card.closest('.task-list') : null;
              var colId = list ? list.dataset.columnId : null;
              var estimate = cardEstimateMinutes(card);
              const wasEpic = !!card && card.dataset.kind === CARD_KIND.EPIC;
              if (card && card.parentNode) card.parentNode.removeChild(card);
              if (wasEpic) BoardChrome.refreshMarks();
              else if (colId) updateColumnCount(colId, -1, -estimate);
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
    // ("No task selected"), so Start never posts a stale id. KF-450: the
    // drop is announced, and the next Start runs without a task.
    startClicked: function () {
      var self = this;
      var mode = this.currentModeTab;
      this.validateSelection().then(function (exists) {
        if (!exists) { self.announceDeletedTask(); return; }
        var task = self.selectedTask;
        self.start(mode, task ? task.id : null);
      });
    },

    // keepPopup: the panel stays open (KF-553: Take break, golden timer/27).
    start: function (mode, taskId, keepPopup) {
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
          self.broadcastTimer();
          self.refresh();
          if (!keepPopup) self.closePopup();
          self.refreshTaskCard(retiredTask);
        } else if (res.status === 404 && taskId && self.selectedTask &&
                   String(self.selectedTask.id) === String(taskId)) {
          self.dropSelection(); // KF-001: the task was deleted meanwhile
          self.announceDeletedTask();
        } else {
          toastRefusal(res, 'Could not start the timer');
        }
      });
    },

    announceDeletedTask: function () {
      toast('The selected task was deleted', 'Press Start again to run without a task');
    },

    // KF-424: Start during a break ends it and runs the next pomodoro on the
    // selected task (golden 27).
    skipBreak: function () {
      this.setModeTab('pomodoro');
      this.startClicked();
    },

    startForTask: function (taskId) {
      // Quick-start a Pomodoro from a card; hide the now-obsolete menu.
      var menu = document.getElementById('tm-timer-menu');
      if (menu) menu.hidden = true;
      this.start('pomodoro', taskId);
    },

    startBreak: function (kind, keepPopup) {
      this.start(kind === 'long' ? 'long_break' : 'short_break', null, keepPopup);
    },

    stopClicked: function () {
      var s = this.state;
      if (!s || s.phase === 'idle') return;
      // KanbanFlow: sessions under 20s are discarded with a toast and no
      // "why did you stop?" menu (KF-007).
      // KF-516: a stopwatch stops without a reason menu (golden timer/24).
      if (this.elapsed() < 20 || s.mode === 'stopwatch') {
        this.whyOriginal = s;
        this.stopAndLog(null);
        return;
      }
      this.beginWhy(s.sessionId, s.taskId, s.taskName, s.mode, s.remainingSeconds, s.totalSeconds);
    },

    // KF-465: the panel shows the idle timer when nothing runs, and the
    // idle stopwatch when the Stopwatch tab is picked during a break (its
    // Start then ends the break).
    panelShowsIdle: function () {
      var s = this.state;
      if (!s || s.phase === 'idle') return true;
      var isBreak = s.mode === 'short_break' || s.mode === 'long_break';
      return isBreak && this.currentModeTab === 'stopwatch';
    },

    // KF-427: the waiting "00:00" shows on even seconds, hides on odd ones.
    blinkOn: function () {
      return this.elapsed() % 2 === 0;
    },

    // KF-006: a pomodoro past its duration waits for "Take break".
    waitingForBreak: function () {
      var s = this.state;
      return !!(s && s.phase !== 'idle' && s.mode === 'pomodoro' && this.remaining() <= 0);
    },

    // KF-006/KF-425: at 00:00 a session chimes once and stays: a pomodoro
    // waits for "Take break", a break stays on "Break time 00:00" with
    // Start (golden 29). The server keeps both running, so a reload lands
    // in the same state.
    sessionReachedZero: function () {
      var s = this.state;
      if (this.chimedFor === s.startedAt) return;
      this.chimedFor = s.startedAt;
      this.playChime();
      var popup = document.getElementById('timer-popup');
      if (popup && !popup.hidden) this.renderPopup();
    },

    // Stop the live session; resolves to the stop result or null. KF-015: a
    // completed session carries no stop reason. KF-453: the request names
    // the session, so a stop another tab already made reads as success.
    stopSession: function (completed) {
      var payload = completed ? { completed: true } : {};
      if (this.state && this.state.startedAt != null) payload.started_at = this.state.startedAt;
      // KF-499: the session id tells apart two sessions started in one second.
      if (this.state && this.state.sessionId != null) payload.session_id = this.state.sessionId;
      // KF-535: a stop that names no session is never sent; the tab has none to stop.
      if (payload.started_at == null && payload.session_id == null) {
        return Promise.resolve({ already_stopped: true });
      }
      return fetch('/api/timer/stop', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify(payload),
      }).then(function (res) { return timerJson(res, 'Could not stop the timer'); })
        .catch(function () { return null; /* the next status poll resyncs */ });
    },

    // KF-453: other tabs resync the moment this one starts, stops or
    // retargets the timer (a storage event fires in every other tab).
    broadcastTimer: function () {
      try {
        window.localStorage.setItem('pomb.timerSync', Date.now() + ':' + Math.random());
      } catch (e) { /* the 15 s poll still resyncs */ }
    },

    // KF-453: follow another tab's timer changes and task selection.
    watchOtherTabs: function () {
      var self = this;
      window.addEventListener('storage', function (e) {
        if (e.key === 'pomb.timerSync') {
          self.refresh();
        } else if (e.key === 'pomb.timerTask') {
          self.selectedTask = self.loadSelectedTask();
          self.updateCardIndicators();
          var popup = document.getElementById('timer-popup');
          if (popup && !popup.hidden) self.renderPopup();
          self.renderPill();
        }
      });
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
      var popup = document.getElementById('timer-popup');
      var keepPopup = !!popup && !popup.hidden;
      this.stopSession(true).then(function (data) {
        // Another tab took the break first, so follow it. A refused stop starts no break.
        if (!data || data.already_stopped) { self.refresh(); return; }
        self.broadcastTimer();
        self.refreshTaskCard(loggedTask);
        fetch('/api/timer/status', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
          .then(function (res) { return jsonIfJson(res); })
          .then(function (status) {
            var count = status ? status.pomodoro_count : 0;
            self.startBreak(count > 0 && count % every === 0 ? 'long' : 'short', keepPopup);
          });
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
      // KF-194: dismissing the why menu without picking a reason (‹/Esc)
      // must still finalize the stop — otherwise the server timer keeps
      // running while the UI looks idle (pill and popup disagree).
      if (this.whyPending) {
        this.whyPending = false;
        this.stopAndLog(null);
      }
      if (menu) menu.hidden = true;
    },

    stopAndLog: function (reason) {
      var self = this;
      var loggedTask = this.workTaskId();
      // KF-452: a stop finished from the why menu returns to the panel.
      var menu = document.getElementById('why-stop-menu');
      var fromMenu = !!(menu && !menu.hidden);
      self.whyPending = false; // the stop is being finalized; a later
      // closeWhyMenu (e.g. from the fetch callback) must not re-trigger it.
      var payload = {};
      if (reason) payload.reason = reason;
      // KF-453: name the session being stopped (the one the why menu opened
      // for), so a stop another tab already made reads as success.
      var session = this.whyOriginal || this.state;
      if (session && session.startedAt != null) payload.started_at = session.startedAt;
      if (session && session.sessionId != null) payload.session_id = session.sessionId;
      this.whyOriginal = null;
      // KF-535: a stop that names no session is never sent; the tab has none to stop.
      var request = payload.started_at == null && payload.session_id == null
        ? Promise.resolve({ already_stopped: true })
        : fetch('/api/timer/stop', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          credentials: 'same-origin',
          body: JSON.stringify(payload),
        }).then(function (res) { return timerJson(res, 'Could not stop the timer'); });
      request
        .then(function (data) {
          if (!data) { self.refresh(); return; }
          self.closeWhyMenu();
          self.hideCardLive(); // KF-103: session over — badge reverts
          self.refresh().then(function () {
            var popup = document.getElementById('timer-popup');
            if (fromMenu && popup && popup.hidden) self.togglePopup();
          });
          // KF-007: the server discards sessions under 20s — say so with
          // KanbanFlow's toast instead of pretending the stop was logged.
          if (data && !data.already_stopped) self.broadcastTimer();
          if (data && data.discarded) {
            toast('Session discarded', 'Session lasted less than 20 seconds');
          } else if (data && !data.already_stopped) {
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
          paintSelection();
          applyCardSecState(fresh);
          if (window.BoardChrome && BoardChrome.refreshMarks) BoardChrome.refreshMarks();
          self.updateCardIndicators();
          self.updateCardLive();
          if (window.BoardChrome && BoardChrome.isFiltering && BoardChrome.isFiltering()) BoardChrome.applyFilter();
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
      // The selected card's own border is transparent; the shade is its variable.
      var shade = card ? window.getComputedStyle(card).getPropertyValue('--taskColorBorder').trim() : '';
      var nameInput = document.getElementById('modal-name');
      if (!name && nameInput && String(modalTaskId()) === String(taskId)) name = nameInput.value;
      return {
        id: String(taskId),
        name: name || '',
        color: card ? shade || window.getComputedStyle(card).borderTopColor : null,

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
        if (status) {
          self.broadcastTimer();
          self.updateFromStatus(status);
        }
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

  // KF-414: a stored due_at may use RFC3339's lowercase "t"/"z" or a
  // space separator, which Safari's Date parser rejects; normalize first.
  function parseDueAt(s) {
    var v = String(s == null ? '' : s).trim()
      .replace(/^(\d{4}-\d{2}-\d{2})[Tt ](?=\d)/, '$1T')
      .replace(/z$/, 'Z');
    return new Date(v);
  }

  // "5:43 PM", or "17:43" on the account's 24-hour clock.
  function clockTime(d) {
    return clockText(d);
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

  // KF-389: the manual time dialogs' Duration readout. KanbanFlow shows
  // "0h" for an empty or zero-length range; "—" means To is before From.
  function durationLabel(from, to) {
    if (!from || !to) return '0h';
    var mins = diffMinutes(from, to);
    if (mins !== null) return fmtDuration(mins);
    var seconds = function (s) {
      var p = s.split(':').map(Number);
      return p[0] * 3600 + p[1] * 60 + (p[2] || 0);
    };
    return seconds(from) === seconds(to) ? '0h' : '—';
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
    var startOffset = daysIntoWeek(first);
    var daysInMonth = new Date(year, month + 1, 0).getDate();
    var monthName = first.toLocaleString('en-US', { month: 'long', year: 'numeric' });
    var html = '<div class="mt-cal-head"><button type="button" data-cal-prev>&lt;</button>' +
      '<span>' + monthName + '</span><button type="button" data-cal-next>&gt;</button></div>' +
      '<div class="mt-cal-grid">';
    weekdaysFromWeekStart(['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat']).forEach(function (d) { html += '<span class="mt-cal-dow">' + d + '</span>'; });
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

  // KF-460: task names repeat across boards, so the dialog's own task wins
  // while its name is unchanged, and a typed name must match one task.
  function resolveTypedTask(name, ownId, ownName) {
    if (ownId && (!name || name === ownName)) return ownId;
    var ids = ManualTime.taskIdsByName[name] || [];
    return ids.length === 1 ? ids[0] : null;
  }

  function typedTaskError(name) {
    var ids = ManualTime.taskIdsByName[name] || [];
    return ids.length > 1
      ? 'Several tasks are named "' + name + '"; add the time from the task itself.'
      : 'Select a task from the list.';
  }

  var ManualTime = {
    taskId: null,
    taskName: null,
    taskIdsByName: Object.create(null),
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
      return fetchAllTasks()
        .then(function (tasks) {
          var list = document.getElementById('mt-task-list');
          if (!list) return;
          var html = '';
          var byName = Object.create(null);
          (tasks || []).forEach(function (t) {
            if (!byName[t.name]) {
              byName[t.name] = [];
              html += '<option value="' + escapeHtml(t.name) + '"></option>';
            }
            byName[t.name].push(t.id);
          });
          self.taskIdsByName = byName;
          list.innerHTML = html;
        })
        .catch(function () { /* datalist stays as-is on failure */ });
    },
    resolveTaskId: function () {
      return resolveTypedTask(document.getElementById('mt-task').value.trim(), this.taskId, this.taskName);
    },
    updateDuration: function () {
      var from = document.getElementById('mt-from').value;
      var to = document.getElementById('mt-to').value;
      var el = document.getElementById('mt-duration');
      el.textContent = durationLabel(from, to);
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
      if (!taskId) { this.showError(typedTaskError(document.getElementById('mt-task').value.trim())); return; }
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
      return resolveTypedTask(document.getElementById('ee-task').value.trim(), this.taskId, this.taskName);
    },
    updateDuration: function () {
      var from = document.getElementById('ee-from').value;
      var to = document.getElementById('ee-to').value;
      var el = document.getElementById('ee-duration');
      if (!from || !to) { el.textContent = '0h'; return; }
      var secs = diffSeconds(from, to);
      // KF-389: an equal From and To reads "0h", like the add dialog.
      el.textContent = secs === null ? durationLabel(from, to)
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
      if (!taskId) { this.showError(typedTaskError(document.getElementById('ee-task').value.trim())); return; }
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
          else if (logOverlay && !logOverlay.hidden) TimerLogPage.reload();
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

  var columnKeyHints = [];

  function clearColumnHints() {
    var had = columnKeyHints.length > 0;
    columnKeyHints.forEach(function (hint) { hint.remove(); });
    columnKeyHints = [];
    return had;
  }

  function showColumnHints() {
    clearColumnHints();
    var adds = document.querySelectorAll('#column-headers-row .columnHeader-add[data-add-task-for]');
    Array.prototype.slice.call(adds, 0, 9).forEach(function (add, i) {
      var hint = document.createElement('span');
      hint.className = 'column-key-hint';
      hint.textContent = String(i + 1);
      add.closest('.columnHeader').appendChild(hint);
      columnKeyHints.push(hint);
    });
    return columnKeyHints.length > 0;
  }

  function pickColumnHint(digit) {
    var adds = document.querySelectorAll('#column-headers-row .columnHeader-add[data-add-task-for]');
    var add = adds[Number(digit) - 1];
    clearColumnHints();
    if (add) add.click();
    return !!add;
  }

  function openBoardList() {
    var dropdown = document.getElementById('boards-dropdown');
    var button = document.getElementById('boards-dropdown-btn');
    var search = document.getElementById('bs-search');
    if (!dropdown || !button || !search) return false;
    if (dropdown.hidden) button.click();
    search.focus();
    return true;
  }

  function reportsMenuKey(e) {
    var sub = document.getElementById('reports-submenu');
    if (!sub || sub.hidden || (e.key !== 'ArrowDown' && e.key !== 'ArrowUp')) return false;
    var items = Array.prototype.slice.call(sub.querySelectorAll('[data-report]'));
    if (!items.length) return false;
    var at = items.indexOf(document.activeElement);
    var step = e.key === 'ArrowDown' ? 1 : -1;
    var next = at === -1 ? (step === 1 ? 0 : items.length - 1) : (at + step + items.length) % items.length;
    items[next].focus();
    return true;
  }

  function boardLetter(key) {
    if (key === 'a') return showColumnHints();
    if (key === 'b') return openBoardList();
    if (key === 'f') return BoardChrome.openFilter();
    if (key === 'r') return BoardChrome.openReports();
    if (key === 'u') return BoardChrome.showAllMembers();
    if (key === 'h') return !!document.getElementById('help-dialog') && BoardChrome.openHelp();
    if (key === 'm') {
      var popup = document.getElementById('timer-popup');
      if (!popup || popup.hidden) return false;
      TimerUI.switchModeTab();
      return true;
    }
    if (key === 's') return TimerUI.runKey();
    return false;
  }

  var SHORTCUT_DIALOGS = '.dlg-overlay:not([hidden]), .addTask-overlay:not([hidden])';

  function timerShortcut(shift) {
    if (document.querySelector(SHORTCUT_DIALOGS)) return false;
    if (shift) {
      if (!document.getElementById('timer-select-pill')) return false;
      TimerUI.beginSelect();
      return true;
    }
    TimerUI.togglePopup();
    return true;
  }

  function onBoardShortcut(e, key) {
    if (columnKeyHints.length) {
      if (/^[1-9]$/.test(e.key)) {
        e.preventDefault();
        pickColumnHint(e.key);
        return true;
      }
      clearColumnHints();
    }
    if (document.querySelector(SHORTCUT_DIALOGS + ', .menu-floating:not([hidden])')) {
      return false;
    }
    if (!boardLetter(key)) return false;
    e.preventDefault();
    return true;
  }

  var MODAL_LETTER_ACTS = {
    c: 'pick-color',
    d: 'add-description',
    f: 'add-attachment',
    k: 'add-comment',
    l: 'add-label',
    m: 'add-member',
    r: 'add-relation',
    s: 'add-subtask',
    u: 'add-duedate',
  };

  function modalLetter(modal, key) {
    if (document.querySelector('.dlg-overlay:not([hidden])')) return false;
    if (key === 'a') {
      var add = document.querySelector('[data-tm-menu="tm-add-menu"]');
      if (!add) return false;
      add.click();
      return true;
    }
    if (key === 'n') {
      var name = document.getElementById('modal-name');
      if (!name) return false;
      name.focus();
      name.select();
      return true;
    }
    var act = MODAL_LETTER_ACTS[key];
    if (!act) return false;
    var source = modal.querySelector('[data-tm-act="' + act + '"]');
    if (!source) return false;
    modalAction(act, source);
    return true;
  }

  function showShortcutsTab(dialog, name) {
    Array.prototype.forEach.call(dialog.querySelectorAll('[data-shortcuts-panel]'), function (panel) {
      panel.hidden = panel.getAttribute('data-shortcuts-panel') !== name;
    });
  }

  document.addEventListener('change', function (e) {
    if (e.target.id !== 'shortcuts-tab') return;
    showShortcutsTab(document.getElementById('shortcuts-dialog'), e.target.value);
  });

  document.addEventListener('click', function (e) {
    if (e.target.closest && e.target.closest('#shortcuts-print')) window.print();
  });

  document.addEventListener('keydown', function (e) {
    var modal = document.querySelector('.task-modal[data-task-id]');
    var inField = !!(e.target && e.target.closest &&
      e.target.closest('input, textarea, select, [contenteditable]'));
    if (!e.metaKey && !e.ctrlKey && !e.altKey && reportsMenuKey(e)) {
      e.preventDefault();
      return;
    }
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
    if (columnKeyHints.length && !/^([1-9]|Shift|Control|Alt|Meta|Escape)$/.test(e.key)) clearColumnHints();
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    // Escape must work even with focus inside a field (dialog inputs etc.).
    if (e.key === 'Escape') { handleEscape(); return; }
    if (inField) return;
    if (document.querySelector(SHORTCUT_DIALOGS)) return;
    var key = e.key.toLowerCase();
    if (key === 't') {
      if (modal) {
        var timerBtn = document.querySelector('[data-tm-menu="tm-timer-menu"]');
        if (timerBtn) timerBtn.click();
      } else {
        timerShortcut(e.shiftKey);
      }
    } else if (key === 'p') {
      // KF-383: with a task open, P opens the modal's Reports menu, as T
      // opens its Timer menu.
      if (modal) {
        var reportsBtn = document.querySelector('[data-tm-menu="tm-reports-menu"]');
        if (reportsBtn) reportsBtn.click();
      } else {
        openReportsMenu();
      }
    } else if (key === 'y') {
      if (document.getElementById('manual-time-overlay')) {
        var yId = modalTaskId();
        var yNameInput = document.getElementById('modal-name');
        ManualTime.open(yId, yNameInput ? yNameInput.value : null);
      }
    } else if (key === 'e') {
      if (modal) openEstimateDialog();
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
    } else if (modal) {
      if (modalLetter(modal, key)) e.preventDefault();
    } else {
      onBoardShortcut(e, key);
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
    open: function (source) {
      var id = modalTaskId();
      if (!id) return;
      this.taskId = id;
      var self = this;
      var detailP = api('/api/tasks/' + encodeURIComponent(id), 'GET')
        .then(function (res) { return res.ok ? res.json() : Promise.reject(); });
      var suggP = api('/api/boards/' + encodeURIComponent(modalBoardId()) + '/labels', 'GET')
        .then(function (res) { return res.ok ? res.json() : []; })
        .catch(function () { return []; });
      Promise.all([detailP, suggP]).then(function (results) {
        self.labels = (results[0].labels || []).slice();
        self.suggestions = results[1] || [];
        self.render();
        document.getElementById('labels-input').value = '';
        self.showAll(false);
        anchorPopover(document.getElementById('labels-dialog'),
          source || document.querySelector('#modal-labels-row .tm-member-add'));
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
      var all = document.getElementById('labels-all');
      all.innerHTML = '';
      this.suggestions.concat(this.labels.filter(function (l) { return self.suggestions.indexOf(l) === -1; }))
        .sort()
        .forEach(function (l) {
          var row = document.createElement('label');
          row.className = 'labels-all-row';
          var box = document.createElement('input');
          box.type = 'checkbox';
          box.checked = self.labels.indexOf(l) !== -1;
          box.addEventListener('change', function () {
            if (box.checked) self.add(l); else self.remove(l);
          });
          row.appendChild(box);
          row.appendChild(document.createTextNode(l));
          all.appendChild(row);
        });
      // KF-269: KanbanFlow's suggestions-area empty text is verbatim
      // "No recently used labels exist" (GM-080, GM-158).
      if (!sug.children.length) {
        sug.innerHTML = '<span class="empty-note">No recently used labels exist</span>';
      }
    },
    showAll: function (on) {
      document.getElementById('labels-all').hidden = !on;
      document.getElementById('labels-list-btn').setAttribute('aria-expanded', on ? 'true' : 'false');
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
    // The stored due_at and the Date/Time fields it filled, so an
    // unchanged save sends the stored instant back instead of rebuilding
    // it from the minute-precision local wall clock.
    // KF-505: a 280 px popover; the due chip anchors the edit, the Add button
    // anchors a new due date.
    place: function (source, subtask) {
      var overlay = document.getElementById('duedate-dialog');
      var dlg = overlay.querySelector('.dlg');
      dlg.classList.toggle('duedate-dlg--add', !!subtask);
      overlay.classList.add('dlg-overlay--popover');
      dlg.style.left = '';
      dlg.style.top = '';
      overlay.hidden = false;
      var w = dlg.offsetWidth;
      var h = dlg.offsetHeight;
      var chip = source && source.closest ? source.closest('.tm-due-chip') : null;
      var left;
      var top;
      if (chip) {
        // The popover's left edge sits 3 px left of the modal's content
        // edge (golden dark/modal/23); it opens below the chip.
        var modalRect = chip.closest('.task-modal').getBoundingClientRect();
        left = modalRect.left + 27;
        top = chip.getBoundingClientRect().bottom + 4;
      } else if (subtask && document.querySelector('.task-modal')) {
        // KF-528: a subtask due date opens 297 px right of the modal's left
        // edge, 79 px below its top (golden modal/22).
        var taskModal = document.querySelector('.task-modal').getBoundingClientRect();
        left = taskModal.left + 297;
        top = taskModal.top + 79;
      } else {
        var add = document.querySelector('.task-modal-iconbar [data-tm-menu="tm-add-menu"]');
        var circle = add ? add.querySelector('.tm-icon') : null;
        var a = circle ? circle.getBoundingClientRect() : { right: window.innerWidth - w - 10, top: 50 };
        left = a.right + 10;
        top = a.top + 7;
      }
      dlg.style.left = Math.max(0, Math.min(left, window.innerWidth - w)) + 'px';
      dlg.style.top = Math.max(0, Math.min(top, window.innerHeight - h)) + 'px';
    },
    loaded: null,
    // KF-545: a subtask's due date reuses the dialog with Date and Time only;
    // `subtaskId` is set while it edits a subtask instead of the task.
    subtaskId: null,
    setSubtaskMode: function (on) {
      document.getElementById('dd-column').closest('label').hidden = on;
      document.getElementById('dd-repeat').closest('label').hidden = on;
    },
    open: function (source, subtaskId) {
      var id = modalTaskId();
      if (!id) return;
      this.taskId = id;
      this.subtaskId = subtaskId || null;
      this.setSubtaskMode(!!subtaskId);
      var self = this;
      api('/api/tasks/' + encodeURIComponent(id), 'GET')
        .then(function (res) { return res.ok ? res.json() : Promise.reject(); })
        .then(function (detail) {
          if (subtaskId) {
            var sub = (detail.subtasks || []).filter(function (s) { return s.id === subtaskId; })[0];
            if (!sub) return Promise.reject();
            detail = { due_at: sub.due_at, due_repeat: null, due_target_column_id: null };
          }
          var dateInput = document.getElementById('dd-date');
          var timeInput = document.getElementById('dd-time');
          // KF-259: a new due date starts today.
          dateInput.value = isoDate(new Date());
          // KF-404: a new due date starts at the board's Due date time.
          timeInput.value = document.getElementById('duedate-dialog').dataset.defaultTime || '17:00';
          self.loaded = null;
          if (detail.due_at) {
            var d = parseDueAt(detail.due_at);
            if (!isNaN(d.getTime())) {
              dateInput.value = isoDate(d);
              timeInput.value = toTimeStr(d);
              self.loaded = { dueAt: detail.due_at, date: dateInput.value, time: timeInput.value };
            }
          }
          // KF-328: KanbanFlow's repeat is a checkbox (GM-081), not free text.
          document.getElementById('dd-repeat').checked = !!detail.due_repeat;
          document.getElementById('dd-delete').hidden = !detail.due_at;
          document.getElementById('dd-title').textContent =
            detail.due_at ? 'Edit due date' : 'Add due date';
          document.getElementById('dd-add').textContent = detail.due_at ? 'Update' : 'Add';
          self.renderColumnList(id, detail.due_at ? detail.due_target_column_id : null);
          self.renderCalendar();
          self.place(source, !!subtaskId);
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
    // KF-259: KanbanFlow's Target column select: "Any archived column",
    // then the runtime columns after the task's own column (plus a stored
    // target outside them), with the stored target selected.
    renderColumnList: function (id, targetId) {
      var sel = document.getElementById('dd-column');
      sel.dataset.taskId = id;
      sel.innerHTML = '';
      var any = document.createElement('option');
      any.value = '';
      any.textContent = 'Any archived column';
      sel.appendChild(any);
      var card = document.querySelector('.task-card[data-task-id="' + cssEscape(id) + '"]');
      var ownList = card ? card.closest('.task-list') : null;
      var ownCol = ownList ? ownList.getAttribute('data-column-id') : null;
      var ths = Array.prototype.slice.call(document.querySelectorAll('.columnHeader[data-column-id]'));
      var cols = ths.map(function (th) { return { id: th.getAttribute('data-column-id'), name: th.dataset.columnName }; });
      function fill(list, own) {
        var ownIndex = list.findIndex(function (c) { return c.id === own; });
        list.forEach(function (c, i) {
          if (i <= ownIndex && c.id !== targetId) return;
          var opt = document.createElement('option');
          opt.value = c.id;
          opt.textContent = c.name || 'Column';
          sel.appendChild(opt);
        });
        sel.value = targetId && sel.querySelector('option[value="' + cssEscape(targetId) + '"]') ? targetId : '';
      }
      fill(cols, ownCol);
      var bid = modalBoardId();
      if (cols.length || !bid) return;
      Promise.all([
        fetchJson('/api/boards/' + encodeURIComponent(bid) + '/columns'),
        fetchJson('/api/tasks/' + encodeURIComponent(id)),
      ]).then(function (loaded) {
        if (!sel.isConnected || sel.dataset.taskId !== id) return;
        sel.innerHTML = '';
        sel.appendChild(any);
        fill(loaded[0] || [], loaded[1].column_id);
      }).catch(function () { toast('Could not load board columns.'); });
    },
    // The PATCH body for the dialog fields; null when the date or time is
    // missing or invalid (KF-417: the dialog requires both).
    collect: function () {
      var date = document.getElementById('dd-date').value;
      var time = document.getElementById('dd-time').value;
      // KF-328: KanbanFlow's repeat is a checkbox (GM-081).
      var repeat = document.getElementById('dd-repeat').checked ? 'every week' : null;
      if (!date || !time) return null;
      var target = document.getElementById('dd-column').value || null;
      var loaded = this.loaded;
      var subOnly = !!this.subtaskId;
      if (loaded && loaded.date === date && loaded.time === time) {
        return subOnly ? { due_at: loaded.dueAt }
          : { due_at: loaded.dueAt, due_repeat: repeat, due_target_column_id: target };
      }
      var d = new Date(date + 'T' + (time.length === 5 ? time + ':00' : time));
      if (isNaN(d.getTime())) return null;
      return subOnly ? { due_at: d.toISOString() }
        : { due_at: d.toISOString(), due_repeat: repeat, due_target_column_id: target };
    },
    // Writes only the open task; the Target column select sets that
    // task's due_target_column_id (KF-259), never which tasks are written.
    save: function (patch, verb) {
      var self = this;
      if (!this.taskId) return;
      var url = '/api/tasks/' + encodeURIComponent(this.taskId) +
        (this.subtaskId ? '/subtasks/' + encodeURIComponent(this.subtaskId) : '');
      api(url, 'PATCH', patch).then(function (res) {
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
      var self = this;
      var patch = this.collect();
      if (!patch) { showAlertDialog('Invalid date or time'); return; }
      var verb = this.loaded ? 'Due date updated' : 'Due date added';
      // KF-432: a stored due date saved unchanged is not asked about again.
      var changed = !this.loaded || patch.due_at !== this.loaded.dueAt;
      if (changed && new Date(patch.due_at).getTime() < Date.now()) {
        showPastDueConfirm(function () { self.save(patch, verb); });
        return;
      }
      this.save(patch, verb);
    },
    // KF-401: Delete removes the open task's due date and its repeat and
    // done state.
    remove: function () {
      this.save(this.subtaskId ? { due_at: null }
        : { due_at: null, due_repeat: null, due_done: false }, 'Due date deleted');
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

  var TimelineDialog = (function () {
    var DOW = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
    var taskId = null;
    var afterSave = null;
    var loaded = false;
    var startISO = '';
    var endISO = '';
    var active = 'start';
    var left = { y: 0, m: 0 };
    var right = { y: 0, m: 0 };

    function toISO(d) {
      return d.getFullYear() + '-' + String(d.getMonth() + 1).padStart(2, '0') + '-' +
        String(d.getDate()).padStart(2, '0');
    }
    function parseISO(s) {
      var m = /^(\d{4})-(\d{2})-(\d{2})$/.exec((s || '').trim());
      if (!m) return null;
      var d = new Date(+m[1], +m[2] - 1, +m[3]);
      return (d.getFullYear() === +m[1] && d.getMonth() === +m[2] - 1 &&
        d.getDate() === +m[3]) ? d : null;
    }
    function addDays(iso, days) {
      var d = parseISO(iso);
      return toISO(new Date(d.getFullYear(), d.getMonth(), d.getDate() + days));
    }
    function defaultRange(todayISO, dueISO) {
      if (dueISO && parseISO(dueISO) && dueISO >= todayISO) return [todayISO, dueISO];
      return [todayISO, addDays(todayISO, 7)];
    }
    function problem(start, end) {
      if (!parseISO(start) || !parseISO(end)) return 'Invalid start or end date';
      if (end < start) return 'The end date cannot be before the start date';
      return null;
    }
    function shift(ym, dir) {
      var m = ym.m + dir;
      var y = ym.y;
      while (m < 0) { m += 12; y--; }
      while (m > 11) { m -= 12; y++; }
      ym.y = y;
      ym.m = m;
    }
    function qs(id) { return document.getElementById(id); }

    function renderCal(el, ym) {
      var first = new Date(ym.y, ym.m, 1);
      var lead = daysIntoWeek(first);
      var daysInMonth = new Date(ym.y, ym.m + 1, 0).getDate();
      var daysInPrev = new Date(ym.y, ym.m, 0).getDate();
      var monthName = first.toLocaleString('en-US', { month: 'long', year: 'numeric' });
      var html = '<div class="mt-cal-head"><button type="button" data-nav="-1" aria-label="Previous month">&lt;</button>' +
        '<span>' + monthName + '</span><button type="button" data-nav="1" aria-label="Next month">&gt;</button></div>' +
        '<div class="mt-cal-grid">';
      weekdaysFromWeekStart(DOW).forEach(function (d) { html += '<span class="mt-cal-dow">' + d + '</span>'; });
      var cells = [];
      var i;
      for (i = lead - 1; i >= 0; i--) cells.push({ d: daysInPrev - i, other: true, off: -1 });
      for (i = 1; i <= daysInMonth; i++) cells.push({ d: i, other: false, off: 0 });
      var trail = (7 - (cells.length % 7)) % 7;
      for (i = 1; i <= trail; i++) cells.push({ d: i, other: true, off: 1 });
      cells.forEach(function (c) {
        var day = toISO(new Date(ym.y, ym.m + c.off, c.d));
        var cls = 'mt-cal-day' + (c.other ? ' other-month' : '') +
          ((!c.other && (day === startISO || day === endISO)) ? ' selected' : '');
        html += '<button type="button" class="' + cls + '" data-date="' + day + '">' + c.d + '</button>';
      });
      el.innerHTML = html + '</div>';
      el.querySelector('[data-nav="-1"]').addEventListener('click', function () { shift(ym, -1); renderBoth(); });
      el.querySelector('[data-nav="1"]').addEventListener('click', function () { shift(ym, 1); renderBoth(); });
      el.querySelectorAll('.mt-cal-day').forEach(function (btn) {
        btn.addEventListener('click', function () { pickDay(btn.getAttribute('data-date')); });
      });
    }
    function renderBoth() {
      renderCal(qs('tl-cal-left'), left);
      renderCal(qs('tl-cal-right'), right);
    }
    function syncFields() {
      qs('tl-start').value = startISO;
      qs('tl-end').value = endISO;
    }
    function pickDay(day) {
      if (active === 'start') { startISO = day; active = 'end'; } else { endISO = day; active = 'start'; }
      syncFields();
      renderBoth();
    }
    function showMonths() {
      var s = parseISO(startISO);
      var e = parseISO(endISO);
      left = { y: s.getFullYear(), m: s.getMonth() };
      right = { y: e.getFullYear(), m: e.getMonth() };
      if (left.y === right.y && left.m === right.m) shift(right, 1);
    }
    function write(patch, verb) {
      api('/api/tasks/' + encodeURIComponent(taskId), 'PATCH', patch).then(function (res) {
        if (!res.ok) throw new Error('save failed');
      }).then(function () {
        qs('timeline-dialog').hidden = true;
        if (afterSave) {
          afterSave();
        } else {
          modalDirty = true;
          openModal(taskId, true);
        }
        toast(verb + '.');
      }).catch(function () { toast('Could not save the timeline.'); });
    }

    return {
      defaultRange: defaultRange,
      problem: problem,
      open: function (forTaskId, onSaved) {
        var id = forTaskId || modalTaskId();
        if (!id) return;
        taskId = id;
        afterSave = forTaskId ? onSaved : null;
        api('/api/tasks/' + encodeURIComponent(id), 'GET')
          .then(function (res) { return res.ok ? res.json() : Promise.reject(); })
          .then(function (detail) {
            loaded = !!(detail.timeline_start && detail.timeline_end);
            if (loaded) {
              startISO = detail.timeline_start;
              endISO = detail.timeline_end;
            } else {
              var due = detail.due_at ? parseDueAt(detail.due_at) : null;
              var range = defaultRange(toISO(new Date()),
                due && !isNaN(due.getTime()) ? toISO(due) : '');
              startISO = range[0];
              endISO = range[1];
            }
            active = 'start';
            qs('tl-title').textContent = loaded ? 'Edit timeline' : 'Add timeline for task';
            qs('tl-add').textContent = loaded ? 'Update' : 'Add';
            qs('tl-delete').hidden = !loaded;
            syncFields();
            showMonths();
            renderBoth();
            qs('timeline-dialog').hidden = false;
            qs('tl-add').focus();
          })
          .catch(function () { toast('Could not load the timeline.'); });
      },
      setActive: function (which) { active = which === 'end' ? 'end' : 'start'; },
      fieldChanged: function (which) {
        var v = (qs(which === 'end' ? 'tl-end' : 'tl-start').value || '').trim();
        if (parseISO(v)) {
          if (which === 'end') endISO = v; else startISO = v;
          showMonths();
          renderBoth();
        }
        syncFields();
      },
      save: function () {
        var message = problem(startISO, endISO);
        if (message) { showAlertDialog(message); return; }
        write({ timeline_start: startISO, timeline_end: endISO },
          loaded ? 'Timeline updated' : 'Timeline added');
      },
      remove: function () {
        showConfirmDialog('Delete timeline', 'Are you sure you want to delete the timeline?', 'Delete',
          function () { write({ timeline_start: null, timeline_end: null }, 'Timeline deleted'); });
      },
    };
  })();

  const fitCommentBox = (box) => {
    box.style.height = 'auto';
    box.style.height = box.scrollHeight + box.offsetHeight - box.clientHeight + 'px';
  };

  const onCommentKeydown = (e) => {
    if (e.key !== 'Enter' || e.shiftKey || e.isComposing || e.keyCode === 229) return;
    e.preventDefault();
    addModalComment();
  };

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
      .then(function () {
        if (modalWatchedState()) return null;
        return api('/api/tasks/' + encodeURIComponent(id) + '/watch', 'POST', { watched: true })
          .then(function (resp) { if (resp.ok) setModalWatchUI(true); })
          .catch(function () {});
      })
      .then(function () { refreshModal(); toast('Comment added.'); })
      .catch(function () { toast('Could not add the comment.'); });
  }

  function closeCommentEditors() {
    var open = document.querySelectorAll('.tm-comment-editor');
    open.forEach(function (editor) { editor.cancel(); });
    return open.length > 0;
  }

  function editModalComment(commentId) {
    var id = modalTaskId();
    var row = commentId && document.querySelector('#modal-comments .tm-comment[data-comment-id="' + commentId + '"]');
    var bodyBox = row && row.querySelector('.tm-comment-body');
    var raw = bodyBox && bodyBox.querySelector('.tm-md-raw');
    if (!id || !bodyBox || !raw) return;
    closeCommentEditors();
    var saved = Array.prototype.slice.call(bodyBox.childNodes);
    var editor = document.createElement('div');
    editor.className = 'tm-comment-editor';
    var ta = document.createElement('textarea');
    ta.className = 'tm-comment-edit-input';
    ta.maxLength = 5000;
    ta.rows = 3;
    ta.value = raw.textContent;
    ta.setAttribute('aria-label', 'Edit comment');
    var actions = document.createElement('div');
    actions.className = 'tm-comment-edit-actions';
    var save = document.createElement('button');
    save.type = 'button';
    save.className = 'btn btn-primary';
    save.textContent = 'Save';
    var cancel = document.createElement('button');
    cancel.type = 'button';
    cancel.className = 'btn';
    cancel.textContent = 'Cancel';
    actions.appendChild(save);
    actions.appendChild(cancel);
    editor.appendChild(ta);
    editor.appendChild(actions);
    editor.cancel = function () {
      bodyBox.textContent = '';
      saved.forEach(function (node) { bodyBox.appendChild(node); });
    };
    function submit() {
      var body = ta.value.trim();
      if (!body) { ta.focus(); return; }
      api('/api/tasks/' + encodeURIComponent(id) + '/comments/' + encodeURIComponent(commentId), 'PATCH', { body: body })
        .then(function (res) { if (!res.ok) throw new Error('edit failed'); refreshModal(); })
        .catch(function () { toast('Could not save the comment.'); });
    }
    save.addEventListener('click', submit);
    cancel.addEventListener('click', editor.cancel);
    wireMentions(ta);
    bindImagePaste(ta);
    ta.addEventListener('keydown', function (e) {
      if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); e.stopPropagation(); submit(); }
    });
    bodyBox.textContent = '';
    bodyBox.appendChild(editor);
    ta.focus();
    ta.setSelectionRange(ta.value.length, ta.value.length);
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

  var EMOJI_SET = ['\u{1F44D}', '\u{1F44E}', '\u{1F601}', '\u{1F389}', '\u2764\uFE0F'];
  var emojiPop = null;
  var emojiAnchor = null;
  var emojiCommentId = null;

  function closeEmojiPop() {
    if (emojiPop) emojiPop.hidden = true;
    emojiAnchor = null;
    emojiCommentId = null;
  }

  function toggleEmojiPop(btn) {
    if (emojiPop && !emojiPop.hidden && emojiAnchor === btn) { closeEmojiPop(); return; }
    closeAllTmMenus();
    if (!emojiPop) {
      emojiPop = document.createElement('div');
      emojiPop.className = 'tm-emoji-pop';
      emojiPop.hidden = true;
      emojiPop.setAttribute('role', 'menu');
      emojiPop.setAttribute('aria-label', 'Add reaction');
      EMOJI_SET.forEach(function (ch) {
        var b = document.createElement('button');
        b.type = 'button';
        b.setAttribute('data-emoji', ch);
        b.textContent = ch;
        b.setAttribute('aria-label', 'React with ' + ch);
        emojiPop.appendChild(b);
      });
      document.body.appendChild(emojiPop);
    }
    emojiAnchor = btn;
    var row = btn.closest('.tm-comment');
    emojiCommentId = row && row.getAttribute('data-comment-id');
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

  var pendingCommentDraft = null;

  function reactToComment(commentId, emoji) {
    var id = modalTaskId();
    if (!id || !commentId || !emoji) return;
    api('/api/tasks/' + encodeURIComponent(id) + '/comments/' + encodeURIComponent(commentId) + '/reactions', 'POST', { emoji: emoji })
      .then(function (res) { if (!res.ok) throw new Error('react failed'); return true; })
      .catch(function () { toast('Could not save the reaction.'); return false; })
      .then(function (saved) {
        if (!saved) return;
        var composer = document.getElementById('modal-comment-input');
        pendingCommentDraft = composer ? composer.value : null;
        refreshModal();
      });
  }

  function hasFiles(e) {
    var types = e.dataTransfer && e.dataTransfer.types;
    return !!types && Array.prototype.indexOf.call(types, 'Files') !== -1;
  }

  function wireAttachments(overlay) {
    var input = document.getElementById('modal-attachment-file');
    if (input) input.addEventListener('change', function () {
      uploadAttachments(input.files);
      input.value = '';
    });
    var box = overlay.querySelector('.task-modal');
    overlay.addEventListener('dragenter', function (e) {
      if (!hasFiles(e)) return;
      e.preventDefault();
      if (box) box.classList.add('tm-drop-active');
    });
    overlay.addEventListener('dragover', function (e) {
      if (!hasFiles(e)) return;
      e.preventDefault();
      e.dataTransfer.dropEffect = 'copy';
      if (box) box.classList.add('tm-drop-active');
    });
    overlay.addEventListener('dragleave', function (e) {
      if (box && !overlay.contains(e.relatedTarget)) box.classList.remove('tm-drop-active');
    });
    overlay.addEventListener('drop', function (e) {
      if (!hasFiles(e)) return;
      e.preventDefault();
      if (box) box.classList.remove('tm-drop-active');
      uploadAttachments(e.dataTransfer.files);
    });
  }

  function attachmentRefusal(res, name, limitMiB) {
    return res.text().then(function (text) {
      var message = text;
      try { message = JSON.parse(text).message || text; } catch (ignore) {}
      message = message.charAt(0).toUpperCase() + message.slice(1);
      if (res.status === 503) {
        toast('The server is busy with other uploads. Try ' + name + ' again in a moment.');
      } else if (res.status === 413) {
        toast(name + ' is larger than the ' + limitMiB + ' MiB limit.');
      } else {
        toast('Could not upload ' + name + '.', message);
      }
    });
  }

  var PASTE_IMAGE_TYPE = /^image\/(png|jpeg|webp|gif)$/;
  var pastedImages = false;
  var BYTES_PER_MIB = 1048576;

  function imageFiles(list) {
    return Array.prototype.filter.call(list || [], function (file) {
      return PASTE_IMAGE_TYPE.test(file.type);
    });
  }

  function imageAlt(name) {
    return name.replace(/\.[A-Za-z0-9]+$/, '').replace(/[\[\]\\`*_<>\r\n]+/g, ' ').trim() || 'image';
  }

  function insertAtCaret(el, text) {
    var start = el.selectionStart == null ? el.value.length : el.selectionStart;
    var end = el.selectionEnd == null ? start : el.selectionEnd;
    el.value = el.value.slice(0, start) + text + el.value.slice(end);
    el.selectionStart = el.selectionEnd = start + text.length;
    el.dispatchEvent(new Event('input', { bubbles: true }));
  }

  function uploadImage(id, file, label) {
    var ext = file.type.split('/')[1].replace('jpeg', 'jpg');
    var name = /^image\.\w+$/.test(file.name) ? 'pasted-' + Date.now() + '-' + label + '.' + ext : file.name;
    var url = '/api/tasks/' + encodeURIComponent(id) + '/attachments?name=' +
      encodeURIComponent(name) + '&mime=' + encodeURIComponent(file.type);
    return fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/octet-stream' },
      body: file,
    }).then(function (res) {
      if (res.ok) return res.json();
      var input = document.getElementById('modal-attachment-file');
      var max = input ? parseInt(input.getAttribute('data-max-bytes'), 10) : 0;
      return attachmentRefusal(res, name, Math.round(max / BYTES_PER_MIB)).then(function () { return null; });
    }).catch(function () {
      toast('Could not upload ' + name + '.');
      return null;
    });
  }

  function attachPastedImages(el, files) {
    var id = modalTaskId();
    if (!id) return;
    files.reduce(function (done, file, at) {
      return done.then(function () {
        return uploadImage(id, file, at).then(function (attachment) {
          if (!attachment) return;
          modalDirty = true;
          if (!el.isConnected || el.closest('[hidden]') || modalTaskId() !== id) {
            toast(attachment.name + ' was attached, but the text it belonged to is gone.');
            return;
          }
          if (el.id === 'desc-editor-input') pastedImages = true;
          insertAtCaret(el, '![' + imageAlt(file.name) + '](attachment:' + attachment.id + ')\n');
        });
      });
    }, Promise.resolve());
  }

  function bindImagePaste(el) {
    el.addEventListener('paste', function (e) {
      var board = e.clipboardData;
      var files = imageFiles(board && board.files);
      var types = board && board.types ? Array.prototype.slice.call(board.types) : [];
      if (!files.length || types.indexOf('text/plain') !== -1) return;
      e.preventDefault();
      attachPastedImages(el, files);
    });
    el.addEventListener('dragover', function (e) {
      if (hasFiles(e)) e.preventDefault();
    });
    el.addEventListener('drop', function (e) {
      var dropped = e.dataTransfer && e.dataTransfer.files;
      var files = imageFiles(dropped);
      if (!files.length) return;
      e.preventDefault();
      e.stopPropagation();
      if (dropped.length > files.length) toast('Only images can be dropped into text. The other files were skipped.');
      attachPastedImages(el, files);
    });
  }

  function uploadAttachments(files) {
    var id = modalTaskId();
    if (!id || !files || !files.length) return;
    var input = document.getElementById('modal-attachment-file');
    var max = input ? parseInt(input.getAttribute('data-max-bytes'), 10) : 0;
    var limitMiB = Math.round(max / BYTES_PER_MIB);
    var queue = Array.prototype.slice.call(files);
    var uploaded = 0;
    function next() {
      if (!queue.length) {
        if (uploaded) {
          modalDirty = true;
          modalTab = 'files';
          if (modalTaskId() === id) refreshModal();
        }
        return;
      }
      var file = queue.shift();
      if (max && file.size > max) {
        toast(file.name + ' is larger than the ' + limitMiB + ' MiB limit.');
        next();
        return;
      }
      var url = '/api/tasks/' + encodeURIComponent(id) + '/attachments?name=' +
        encodeURIComponent(file.name) + '&mime=' + encodeURIComponent(file.type || 'application/octet-stream');
      fetch(url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/octet-stream' },
        body: file,
      })
        .then(function (res) {
          if (res.ok) { uploaded++; return null; }
          return attachmentRefusal(res, file.name, limitMiB);
        })
        .catch(function () { toast('Could not upload ' + file.name + '.'); })
        .then(next);
    }
    next();
  }

  function deleteAttachmentMessage(refs) {
    if (!refs) return 'Delete this attachment?';
    var where = refs === 1 ? '1 description or comment shows' : refs + ' descriptions and comments show';
    return 'Delete this attachment? ' + where + ' it. Their text keeps the image name.';
  }

  function deleteModalAttachment(attachmentId) {
    var id = modalTaskId();
    if (!id || !attachmentId) return;
    var row = document.querySelector('.tm-attachment[data-attachment-id="' + cssEscape(attachmentId) + '"]');
    var refs = row ? parseInt(row.getAttribute('data-attachment-refs'), 10) || 0 : 0;
    showConfirmDialog('Delete attachment', deleteAttachmentMessage(refs), 'Delete', function () {
      api('/api/tasks/' + encodeURIComponent(id) + '/attachments/' + encodeURIComponent(attachmentId), 'DELETE')
        .then(function (res) {
          if (!res.ok) throw new Error('delete failed');
          modalDirty = true;
          refreshModal();
        })
        .catch(function () { toast('Could not delete the attachment.'); });
    });
  }

  var attachmentRenameId = null;

  function openAttachmentRename(attachmentId) {
    var row = document.querySelector('.tm-attachment[data-attachment-id="' + cssEscape(attachmentId) + '"]');
    if (!row) return;
    var name = row.getAttribute('data-attachment-name') || '';
    var suffix = row.getAttribute('data-attachment-suffix') || '';
    attachmentRenameId = attachmentId;
    var overlay = document.getElementById('attachment-rename-dialog');
    var dlg = overlay.querySelector('.dlg');
    var input = document.getElementById('ar-input');
    input.value = suffix && name.endsWith(suffix) ? name.slice(0, name.length - suffix.length) : name;
    document.getElementById('ar-suffix').textContent = suffix;
    overlay.hidden = false;
    var more = row.querySelector('.tm-attachment-more').getBoundingClientRect();
    dlg.style.left = Math.max(0, Math.min(more.right + 12 - dlg.offsetWidth, window.innerWidth - dlg.offsetWidth)) + 'px';
    dlg.style.top = Math.max(0, Math.min(more.top + 24, window.innerHeight - dlg.offsetHeight)) + 'px';
    input.focus();
    input.select();
  }

  function doRenameAttachment() {
    var id = modalTaskId();
    if (!id || !attachmentRenameId) return;
    var base = document.getElementById('ar-input').value.trim();
    if (!base) { toast('Enter a name for the attachment.'); return; }
    var name = base + document.getElementById('ar-suffix').textContent;
    api('/api/tasks/' + encodeURIComponent(id) + '/attachments/' + encodeURIComponent(attachmentRenameId), 'PATCH', { name: name })
      .then(function (res) {
        document.getElementById('attachment-rename-dialog').hidden = true;
        if (res.ok) { modalDirty = true; refreshModal(); return null; }
        return toastRefusal(res, 'Could not rename the attachment.');
      })
      .catch(function () { toast('Could not rename the attachment.'); });
  }

  var docState = { taskId: null, attachmentId: null, mode: 'edit', text: '', name: '', editedAt: null };

  function docWhen(iso) {
    var when = new Date(iso);
    if (isNaN(when.getTime())) return '';
    var time = when.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
    var now = new Date();
    if (when.toDateString() === now.toDateString()) return 'Today ' + time;
    return when.toLocaleDateString() + ' ' + time;
  }

  function docBase(name) {
    return name.endsWith('.txt') ? name.slice(0, name.length - 4) : name;
  }

  function docRender(text, into) {
    into.textContent = '';
    if (!text.trim()) { into.textContent = 'Nothing to preview'; return Promise.resolve(); }
    return api('/api/markdown/preview', 'POST', { description: text })
      .then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); })
      .then(function (html) { into.innerHTML = html; })
      .catch(function () { into.textContent = 'The preview could not be loaded'; });
  }

  function docShow(mode) {
    docState.mode = mode;
    var editing = mode === 'edit';
    document.getElementById('doc-editor').hidden = !editing;
    document.getElementById('doc-preview').hidden = editing;
    document.getElementById('doc-save').hidden = !editing;
    document.getElementById('doc-edit').hidden = mode !== 'read';
    document.getElementById('doc-name').readOnly = !editing;
    var meta = document.getElementById('doc-meta');
    if (mode === 'version') meta.textContent = 'An earlier version, read only.';
    else if (docState.editedAt) meta.textContent = 'Updated: ' + docWhen(docState.editedAt);
    else meta.textContent = '';
    if (editing) docTab('write');
  }

  function docTab(tab) {
    var preview = tab === 'preview';
    document.querySelectorAll('#document-dialog [data-doc-tab]').forEach(function (t) {
      t.classList.toggle('doc-tab--on', t.getAttribute('data-doc-tab') === tab);
    });
    var text = document.getElementById('doc-text');
    var shown = document.getElementById('doc-preview');
    text.hidden = preview;
    document.getElementById('doc-tools').hidden = preview;
    shown.hidden = !preview;
    if (preview) docRender(text.value, shown);
    else text.focus();
  }

  function docOpenDialog() {
    var overlay = document.getElementById('document-dialog');
    overlay.hidden = false;
    document.getElementById('doc-versions').hidden = true;
    var text = document.getElementById('doc-text');
    text.hidden = false;
    document.getElementById('doc-name').value = docBase(docState.name);
    text.value = docState.text;
    var download = document.getElementById('doc-download');
    var existing = !!docState.attachmentId;
    download.hidden = !existing;
    document.getElementById('doc-history').hidden = !existing;
    if (existing) {
      download.href = '/api/tasks/' + encodeURIComponent(docState.taskId) + '/attachments/' +
        encodeURIComponent(docState.attachmentId) + '/file';
    }
  }

  function newDocument() {
    var id = modalTaskId();
    if (!id) return;
    docState = { taskId: id, attachmentId: null, mode: 'edit', text: '', name: '', editedAt: null };
    docOpenDialog();
    docShow('edit');
    document.getElementById('doc-name').focus();
  }

  function openDocument(attachmentId) {
    var id = modalTaskId();
    if (!id || !attachmentId) return;
    var row = document.querySelector('.tm-attachment[data-attachment-id="' + cssEscape(attachmentId) + '"]');
    var base = '/api/tasks/' + encodeURIComponent(id) + '/attachments/' + encodeURIComponent(attachmentId);
    Promise.all([
      fetch(base + '/file').then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); }),
      fetchJson('/api/tasks/' + encodeURIComponent(id) + '/attachments'),
    ]).then(function (parts) {
      var meta = (parts[1] || []).filter(function (a) { return a.id === attachmentId; })[0] || {};
      docState = {
        taskId: id,
        attachmentId: attachmentId,
        mode: 'read',
        text: parts[0],
        name: meta.name || (row ? row.getAttribute('data-attachment-name') : ''),
        editedAt: meta.edited_at || meta.created_at || null,
      };
      docOpenDialog();
      docShow('read');
      docRender(docState.text, document.getElementById('doc-preview'));
    }).catch(function () { toast('Could not open the document.'); });
  }

  function saveDocument() {
    var id = docState.taskId;
    var text = document.getElementById('doc-text').value;
    var name = document.getElementById('doc-name').value.trim() || 'Unnamed document';
    var base = '/api/tasks/' + encodeURIComponent(id) + '/attachments';
    var request = docState.attachmentId
      ? api(base + '/' + encodeURIComponent(docState.attachmentId) + '/document', 'PUT', { name: name, text: text })
      : api(base + '/document', 'POST', { name: name, text: text });
    request.then(function (res) {
      if (!res.ok) return toastRefusal(res, 'Could not save the document.');
      return res.json().then(function (saved) {
        docState = {
          taskId: id,
          attachmentId: saved.id,
          mode: 'read',
          text: text,
          name: saved.name,
          editedAt: saved.edited_at || saved.created_at,
        };
        docOpenDialog();
        docShow('read');
        docRender(text, document.getElementById('doc-preview'));
        modalDirty = true;
        modalTab = 'files';
        if (modalTaskId() === id) refreshModal();
      });
    }).catch(function () { toast('Could not save the document.'); });
  }

  function showDocumentVersions() {
    var box = document.getElementById('doc-versions');
    if (!box.hidden) { box.hidden = true; return; }
    var base = '/api/tasks/' + encodeURIComponent(docState.taskId) + '/attachments/' +
      encodeURIComponent(docState.attachmentId);
    fetchJson(base + '/versions').then(function (versions) {
      box.textContent = '';
      var current = document.createElement('button');
      current.type = 'button';
      current.textContent = 'Current, ' + docWhen(docState.editedAt);
      current.addEventListener('click', function () {
        document.getElementById('doc-text').value = docState.text;
        docShow('read');
        docRender(docState.text, document.getElementById('doc-preview'));
      });
      box.appendChild(current);
      (versions || []).forEach(function (version) {
        var item = document.createElement('button');
        item.type = 'button';
        item.textContent = docWhen(version.saved_at) + ', ' + version.size + ' bytes';
        item.addEventListener('click', function () {
          fetch(base + '/versions/' + encodeURIComponent(version.id))
            .then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); })
            .then(function (text) {
              docShow('version');
              docRender(text, document.getElementById('doc-preview'));
            })
            .catch(function () { toast('Could not load the version.'); });
        });
        box.appendChild(item);
      });
      box.hidden = false;
    }).catch(function () { toast('Could not load the versions.'); });
  }

  function printDocument() {
    var shown = document.getElementById('doc-preview');
    var run = docState.mode === 'edit'
      ? docRender(document.getElementById('doc-text').value, shown)
      : Promise.resolve();
    run.then(function () {
      var win = window.open('', '_blank');
      if (!win) { toast('Allow pop-ups to print.'); return; }
      win.document.title = document.getElementById('doc-name').value;
      var body = win.document.createElement('div');
      body.innerHTML = shown.innerHTML;
      win.document.body.appendChild(body);
      win.focus();
      win.print();
    });
  }

  function documentTool(tool) {
    var area = document.getElementById('doc-text');
    var start = area.selectionStart;
    var end = area.selectionEnd;
    var chosen = area.value.slice(start, end);
    var wraps = {
      bold: ['**', '**', 'bold text'],
      italic: ['_', '_', 'italic text'],
      link: ['[', '](https://)', 'link text'],
      image: ['![', '](https://)', 'image description'],
      heading: ['## ', '', 'Heading'],
    };
    if (tool === 'help') {
      toast('Markdown help', '# Heading, **bold**, _italic_, [text](url), ![alt](url), - list, `code`');
      return;
    }
    var wrap = wraps[tool];
    if (!wrap) return;
    var inner = chosen || wrap[2];
    area.value = area.value.slice(0, start) + wrap[0] + inner + wrap[1] + area.value.slice(end);
    area.focus();
    area.setSelectionRange(start + wrap[0].length, start + wrap[0].length + inner.length);
  }

  function wireDocumentDialog() {
    var dialog = document.getElementById('document-dialog');
    if (!dialog) return;
    document.getElementById('doc-save').addEventListener('click', saveDocument);
    document.getElementById('doc-edit').addEventListener('click', function () {
      document.getElementById('doc-text').value = docState.text;
      docShow('edit');
    });
    document.getElementById('doc-print').addEventListener('click', printDocument);
    document.getElementById('doc-history').addEventListener('click', showDocumentVersions);
    dialog.addEventListener('click', function (e) {
      var tab = e.target.closest('[data-doc-tab]');
      if (tab) docTab(tab.getAttribute('data-doc-tab'));
      var tool = e.target.closest('[data-doc-tool]');
      if (tool) documentTool(tool.getAttribute('data-doc-tool'));
    });
  }

  function setAttachmentCover(attachmentId, cover) {
    var id = modalTaskId();
    if (!id || !attachmentId) return;
    api('/api/tasks/' + encodeURIComponent(id) + '/attachments/' + encodeURIComponent(attachmentId) + '/cover',
      cover ? 'PUT' : 'DELETE')
      .then(function (res) {
        if (res.ok) { modalDirty = true; modalTab = 'files'; refreshModal(); return null; }
        return toastRefusal(res, 'Could not change the cover image.');
      })
      .catch(function () { toast('Could not change the cover image.'); });
  }

  function openAttachmentLink() {
    var overlay = document.getElementById('attachment-link-dialog');
    if (!overlay) return;
    var dlg = overlay.querySelector('.dlg');
    var name = document.getElementById('al-name');
    var url = document.getElementById('al-url');
    name.value = '';
    url.value = '';
    overlay.hidden = false;
    dlg.style.left = Math.max(0, (window.innerWidth - dlg.offsetWidth) / 2) + 'px';
    dlg.style.top = Math.max(0, (window.innerHeight - dlg.offsetHeight) / 3) + 'px';
    name.focus();
  }

  function doAttachLink() {
    var id = modalTaskId();
    if (!id) return;
    var name = document.getElementById('al-name').value.trim();
    var url = document.getElementById('al-url').value.trim();
    if (!name) { toast('Enter a name for the link.'); return; }
    if (!url) { toast('Enter the link address.'); return; }
    api('/api/tasks/' + encodeURIComponent(id) + '/attachments/link', 'POST', { name: name, url: url })
      .then(function (res) {
        if (res.ok) {
          document.getElementById('attachment-link-dialog').hidden = true;
          modalDirty = true;
          modalTab = 'files';
          refreshModal();
          return null;
        }
        return toastRefusal(res, 'Could not attach the link.');
      })
      .catch(function () { toast('Could not attach the link.'); });
  }

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
        var bid = modalBoardId();
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

    document.addEventListener('click', function (e) {
      if (!e.target.closest('.tm-picker, [data-link-open], [data-umbrella-change]')) {
        closeLinkPicker(false);
      }
    });

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

    // The per-comment ⋮ menu carries Delete; the
    // smiley opens the reaction picker.
    document.addEventListener('click', function (e) {
      var emojiBtn = e.target.closest('[data-comment-emoji]');
      if (emojiBtn) { toggleEmojiPop(emojiBtn); return; }
      var emojiPick = e.target.closest('.tm-emoji-pop button');
      if (emojiPick) {
        var pickedFor = emojiCommentId;
        closeEmojiPop();
        reactToComment(pickedFor, emojiPick.getAttribute('data-emoji'));
        return;
      }
      var pill = e.target.closest('[data-comment-react]');
      if (pill && !document.querySelector('.board-wrap[data-role="viewer"]')) { closeEmojiPop(); reactToComment(pill.getAttribute('data-comment-react'), pill.getAttribute('data-emoji')); return; }
      if (!e.target.closest('.tm-emoji-pop')) closeEmojiPop();
      var cedit = e.target.closest('[data-comment-edit]');
      if (cedit) { closeAllTmMenus(); editModalComment(cedit.getAttribute('data-comment-edit')); }
      var cdel = e.target.closest('[data-comment-delete]');
      if (cdel) { deleteModalComment(cdel.getAttribute('data-comment-delete')); }
      var adel = e.target.closest('[data-attachment-delete]');
      if (adel) { closeAllTmMenus(); deleteModalAttachment(adel.getAttribute('data-attachment-delete')); }
      var dopen = e.target.closest('[data-document-open]');
      if (dopen) { e.preventDefault(); openDocument(dopen.getAttribute('data-document-open')); }
      var acover = e.target.closest('[data-attachment-cover]');
      if (acover) {
        closeAllTmMenus();
        setAttachmentCover(acover.getAttribute('data-attachment-cover'), acover.getAttribute('data-cover') === 'on');
      }
      var aren = e.target.closest('[data-attachment-rename]');
      if (aren) { closeAllTmMenus(); openAttachmentRename(aren.getAttribute('data-attachment-rename')); }
    });
    var renameDialog = document.getElementById('attachment-rename-dialog');
    if (renameDialog) {
      document.getElementById('ar-rename').addEventListener('click', doRenameAttachment);
      document.getElementById('ar-input').addEventListener('keydown', function (e) {
        if (e.key === 'Enter') { e.preventDefault(); doRenameAttachment(); }
      });
      renameDialog.addEventListener('click', function (e) {
        if (e.target === e.currentTarget) e.currentTarget.hidden = true;
      });
    }

    wireDocumentDialog();
    var linkDialog = document.getElementById('attachment-link-dialog');
    if (linkDialog) {
      document.getElementById('al-add').addEventListener('click', doAttachLink);
      linkDialog.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') { e.preventDefault(); doAttachLink(); }
      });
      linkDialog.addEventListener('click', function (e) {
        if (e.target === e.currentTarget) e.currentTarget.hidden = true;
      });
    }

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
      if (e.target.closest('#labels-list-btn')) LabelsDialog.showAll(document.getElementById('labels-all').hidden);
    });

    // Due-date dialog (KF-270: "Add due date" with inline calendar).
    var ddAdd = document.getElementById('dd-add');
    if (ddAdd) ddAdd.addEventListener('click', function () { DueDateDialog.add(); });
    var ddDelete = document.getElementById('dd-delete');
    if (ddDelete) ddDelete.addEventListener('click', function () { DueDateDialog.remove(); });

    var tlAdd = document.getElementById('tl-add');
    if (tlAdd) tlAdd.addEventListener('click', function () { TimelineDialog.save(); });
    var tlDelete = document.getElementById('tl-delete');
    if (tlDelete) tlDelete.addEventListener('click', function () { TimelineDialog.remove(); });
    ['tl-start', 'tl-end'].forEach(function (id, i) {
      var input = document.getElementById(id);
      if (!input) return;
      var which = i === 0 ? 'start' : 'end';
      input.addEventListener('focus', function () { TimelineDialog.setActive(which); });
      input.addEventListener('change', function () { TimelineDialog.fieldChanged(which); });
    });

    // KF-253: description editor dialog.
    var descSave = document.getElementById('desc-editor-save');
    if (descSave) descSave.addEventListener('click', saveDescriptionEditor);
    var descInput = document.getElementById('desc-editor-input');
    if (descInput) bindImagePaste(descInput);
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

  // The visible overlay that stacks on top: dialogs and the timer log.
  function topmostOverlay() {
    var all = document.querySelectorAll(
      '.dlg-overlay:not([hidden]), #timer-log-overlay:not([hidden]), #log-custom-dialog-overlay:not([hidden])');
    var best = null;
    var bestZ = -1;
    all.forEach(function (el) {
      var z = parseInt(window.getComputedStyle(el).zIndex, 10) || 0;
      if (z >= bestZ) { best = el; bestZ = z; }
    });
    return best;
  }

  function reportBackHref() {
    var back = document.querySelector('.bshell-report .bshell-back');
    return back ? back.getAttribute('href') : null;
  }

  function reportEscapeHref() {
    var href = reportBackHref();
    return href && !document.querySelector('#modal-root .task-modal') ? href : null;
  }

  // Unified Escape: submenus, floating menus, dialogs, time dialogs, why
  // menu, then the task modal — innermost surface first.
  function handleEscape() {
    if (clearColumnHints()) return;
    if (typeof closeEmojiPop === 'function' && emojiPop && !emojiPop.hidden) { closeEmojiPop(); return; }
    if (closeLinkPicker(true)) return;
    if (closeAllTmMenus()) return;
    if (hideFloatingMenus()) return;
    // KF-001: Escape leaves the timer's "Select task" mode.
    if (TimerUI.selecting) { TimerUI.endSelect(); return; }
    // KF-504: the topmost open overlay closes alone (highest z-index, the
    // later one in the page on a tie), so Escape preserves the dialog
    // beneath an alert and the timer log beneath an entry dialog.
    var top = topmostOverlay();
    if (top && top.id === 'calendar-dialog' && document.querySelector('#modal-root .task-modal')) top = null;
    if (top) {
      if (top.id === 'labels-dialog' && typeof LabelsDialog !== 'undefined') LabelsDialog.close();
      else if (top.id === 'timer-log-overlay' || top.id === 'log-custom-dialog-overlay') TimerLogPage.escape();
      else top.hidden = true;
      return;
    }
    var why = document.getElementById('why-stop-menu');
    if (why && !why.hidden) { TimerUI.closeWhyMenu(); return; }
    var backHref = reportEscapeHref();
    if (backHref) {
      window.location.href = backHref;
      return;
    }
    // KF-299: two-stage Escape for the task modal — first Esc discards
    // in-progress edits while keeping the modal open; a second Esc with
    // nothing pending closes the modal.
    if (discardModalDrafts()) return;
    closeModal();
  }

  // KF-299: returns true (and clears the drafts) when the open task modal
  // holds unsent edits: the comment composer, the subtask input, or (KF-388)
  // a changed but uncommitted title. The description edits through its own
  // dialog, so it has no discardable draft here.
  function discardModalDrafts() {
    var modal = document.querySelector('#modal-root .task-modal');
    if (!modal) return false;
    var discarded = closeCommentEditors();
    // Restoring the saved title before it blurs keeps its change handler
    // from saving the draft.
    var nameInput = document.getElementById('modal-name');
    if (nameInput && nameInput.value !== nameInput.defaultValue) {
      nameInput.value = nameInput.defaultValue;
      discarded = true;
    }
    var commentTa = document.getElementById('modal-comment-input');
    if (commentTa && commentTa.value.trim() !== '') { commentTa.value = ''; fitCommentBox(commentTa); discarded = true; }
    var subInput = document.getElementById('modal-subtask-input');
    if (subInput && subInput.value.trim() !== '') { subInput.value = ''; discarded = true; }
    return discarded;
  }

  // ---------- timer log page (KF-090) ----------

  // KanbanFlow period presets -> [fromISO, toISO]. Weeks start on the account's first day.
  function periodToRange(preset, customFrom, customTo, relN, relUnit) {
    var now = new Date();
    var today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
    function monday(d) {
      var x = new Date(d);
      x.setDate(x.getDate() - daysIntoWeek(x));
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

  function paintAvatar(el, member) {
    var avatar = member.avatar;
    el.style.cssText = avatar.style;
    el.textContent = '';
    if (avatar.image) {
      var img = document.createElement('img');
      img.src = avatar.image;
      img.alt = '';
      el.appendChild(img);
    } else {
      el.textContent = avatar.text;
    }
  }

  function csvText(value) {
    var text = String(value == null ? '' : value);
    return typeof value === 'string' && /^[=+\-@\t\r]/.test(text) ? "'" + text : text;
  }

  function saveBlob(filename, blob) {
    var a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    setTimeout(function () { URL.revokeObjectURL(a.href); a.remove(); }, 100);
  }

  function csvCell(v) {
    var s = csvText(v);
    return /[",\n\r]/.test(s) ? '"' + s.replace(/"/g, '""') + '"' : s;
  }

  function downloadCsv(filename, rows) {
    var csv = rows.map(function (r) {
      return r.map(function (c) {
        return '"' + csvText(c).replace(/"/g, '""') + '"';
      }).join(',');
    }).join('\r\n');
    saveBlob(filename, new Blob(['﻿' + csv], { type: 'text/csv;charset=utf-8' }));
  }

  var TimerLogPage = (function () {
    var initialized = false;
    var boardsLoaded = false;

    function qs(id) { return document.getElementById(id); }

    // ---------- period select (KF-286, KF-497) ----------
    function currentPeriod() { return { value: qs('log-period-filter').value }; }

    function renderPeriodLabel() {
      var rel = qs('log-custom-relative');
      if (rel) rel.hidden = currentPeriod().value !== 'custom-relative';
    }

    // KF-290: picking Custom (absolute) opens the two-calendar dialog; the
    // log reloads only when the user confirms with Done.
    function periodChanged() {
      renderPeriodLabel();
      if (currentPeriod().value === 'custom-absolute') {
        CustomRange.open();
        return;
      }
      if (currentPeriod().value !== 'custom-relative') loadLog();
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
        var lead = daysIntoWeek(first);
        var daysInMonth = new Date(ym.y, ym.m + 1, 0).getDate();
        var daysInPrev = new Date(ym.y, ym.m, 0).getDate();
        var monthName = first.toLocaleString('en-US', { month: 'long', year: 'numeric' });
        var html = '<div class="mt-cal-head"><button type="button" data-nav="-1" aria-label="Previous month">&lt;</button>' +
          '<span>' + monthName + '</span><button type="button" data-nav="1" aria-label="Next month">&gt;</button></div>' +
          '<div class="mt-cal-grid">';
        weekdaysFromWeekStart(DOW).forEach(function (d) { html += '<span class="mt-cal-dow">' + d + '</span>'; });
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
        },
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
          ? '<div class="log-stopped">Stopped Pomodoro with reason "' +
            escapeHtml(e.interrupt_reason || 'No reason') + '"</div>'
          : '<div class="log-success">Successful Pomodoro</div>';
      }
      if (e.kind === 'stopwatch') {
        return '<div class="log-neutral">Stopwatch session' +
          (e.interrupted && e.interrupt_reason ? ' — ' + escapeHtml(e.interrupt_reason) : '') +
          '</div>';
      }
      // KF-464: a break is not manual time.
      if (e.kind === 'short_break') return '<div class="log-neutral">Short break</div>';
      if (e.kind === 'long_break') return '<div class="log-neutral">Long break</div>';
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
        // KF-428: "Today 27m 1 Pomodoro" — the day, its total and its
        // completed pomodori, the count hidden at 0 ("Today 30s"; goldens 20).
        var seconds = rows.reduce(function (s, e) { return s + (e.seconds || (e.minutes || 0) * 60); }, 0);
        var total = seconds < 60 ? seconds + 's' : fmtDuration(Math.floor(seconds / 60));
        var pomos = rows.filter(function (e) { return e.kind === 'pomodoro' && !e.interrupted; }).length;
        var count = pomos ? '<span class="log-day-total">' + pomos + (pomos === 1 ? ' Pomodoro' : ' Pomodoros') + '</span>' : '';
        html += '<div class="log-day-group">' +
          '<div class="log-day-head"><span class="log-day-title">' + escapeHtml(dayHeader(key)) +
          '</span><span class="log-day-total">' + escapeHtml(total) + '</span>' + count + '</div>';
        rows.forEach(function (e) {
          // KF-461: golden 20's row — a task-colour dot, the bold task name
          // over an italic status line, then the time span, the duration and
          // a ⋮. KF-004: clicking the row opens the Edit entry dialog.
          html += '<div class="log-entry" data-entry-id="' + escapeHtml(e.id) + '">' +
            '<span class="log-dot" style="background:' + escapeHtml(e.task_color || '#9e9e9e') + '"></span>' +
            '<div class="log-main"><div class="log-task" title="' + escapeHtml(e.badge_title) + '">' +
            escapeHtml(e.task_name) + '</div>' +
            entryStatus(e) +
            (e.note ? '<div class="log-note">' + escapeHtml(e.note) + '</div>' : '') +
            (e.labels && e.labels.length > 0 ? '<div class="log-labels">' + e.labels.map(function(l) { return '<span class="tm-label-chip">' + escapeHtml(l) + '</span>'; }).join('') + '</div>' : '') +
            (e.role || e.token_name ? '<div class="log-author">' + escapeHtml([e.role, e.token_name].filter(Boolean).join(' · ')) + '</div>' : '') +
            '</div><span class="log-when">' + escapeHtml(LocalTime.span(e.started_at, e.ended_at, e.time_range)) + '</span>' +
            '<span class="log-dur">' + escapeHtml(fmtDuration(e.minutes, e.duration_display)) + '</span>' +
            '<button type="button" class="log-more" data-edit-entry="' + escapeHtml(e.id) + '" title="Edit entry" aria-label="Edit entry">' +
            '<svg width="4" height="14" viewBox="0 0 4 14" aria-hidden="true" focusable="false"><circle cx="2" cy="2" r="1.7"/><circle cx="2" cy="7" r="1.7"/><circle cx="2" cy="12" r="1.7"/></svg>' +
            '</button></div>';
        });
        html += '<div class="log-add-row"><button type="button" class="log-add-entry" data-open-manual-time' +
          ' data-date="' + escapeHtml(key) + '">Add time entry</button></div></div>';
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
            ? 'Stopped Pomodoro with reason "' + (e.interrupt_reason || '') + '"'
            : 'Successful Pomodoro')
          : e.kind === 'stopwatch' ? 'Stopwatch session'
            : e.kind === 'short_break' ? 'Short break'
            : e.kind === 'long_break' ? 'Long break' : 'Manually added time';
        rows.push([
          e.day_key, e.task_name, e.board_name || '', e.kind,
          fmtDuration(e.minutes), LocalTime.span(e.started_at, e.ended_at, e.time_range), status, e.note || '',
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
        document.getElementById('spent-view-summary').classList.toggle('kf-btn-active', v === 'summary');
        document.getElementById('spent-view-detailed').classList.toggle('kf-btn-active', v === 'detailed');
        loadSpent();
      },
      periodRange: function () {
        var preset = document.getElementById('spent-period').value;
        var map = {
          'last-7': 'last-7', 'last-14': 'last-14', 'last-30': 'last-30',
          'this-week': 'this-week', 'last-week': 'last-week',
          'this-plus-last-week': 'this-plus-last-week',
          'this-month': 'this-month', 'last-month': 'last-month',
        };
        return periodToRange(map[preset] || 'this-plus-last-week');
      },
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
      list.innerHTML = '<p class="settings-hint">Loading&hellip;</p>';
      fetchJson('/api/timer/time-spent?' + p.toString()).then(function (rep) {
        var days = (rep.days || []).filter(function (d) { return d.minutes > 0; });
        var group = qs('spent-group').value;
        if (group === 'role') {
          renderSpentRoles(rep, list);
          return;
        }
        days.sort(function (a, b) {
          return group === 'date-asc' ? (a.date < b.date ? -1 : 1) : (a.date > b.date ? -1 : 1);
        });
        if (!days.length) {
          qs('spent-total').textContent = 'Total: ' + fmtDuration(rep.total_minutes || 0);
          list.innerHTML = '<p class="settings-hint">No entries exist for the given filter.</p>';
          return;
        }
        // KF-292: KanbanFlow-style table — date | bar+percent | tasks | breaks | duration%.
        var total = rep.total_minutes || 0;
        var html = '<div class="bshell-scroll"><table class="bshell-table spent-table"><thead><tr>' +
          '<th>Date</th><th></th><th class="num">Tasks</th><th class="num">Breaks</th><th class="num">Duration</th></tr></thead><tbody>';
        days.forEach(function (d) {
          var pct = total > 0 ? Math.round(d.minutes / total * 100) : 0;
          html += '<tr class="spent-row">' +
            '<td class="spent-date">' + escapeHtml(fmtDayLabel(d.date)) + '</td>' +
            '<td class="spent-bar-cell"><div class="spent-bar-wrap"><div class="spent-bar-track">' +
              '<div class="spent-bar-fill" style="width:' + pct + '%"></div></div>' +
              '<span class="spent-bar-pct">' + pct + '%</span></div></td>' +
            '<td class="num">' + escapeHtml(fmtDuration(d.task_minutes || 0)) + '</td>' +
            '<td class="num">' + escapeHtml(fmtDuration(d.break_minutes || 0)) + '</td>' +
            '<td class="num">' + pct + '%</td></tr>';
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
          '<td class="num">' + escapeHtml(fmtDuration(rep.total_task_minutes || 0)) + '</td>' +
          '<td class="num">' + escapeHtml(fmtDuration(rep.total_break_minutes || 0)) + '</td>' +
          '<td class="num">' + totalPct + '%</td></tr>' +
          '<tr class="spent-footer-total"><td colspan="2"></td>' +
          '<td colspan="3">' + escapeHtml(fmtDuration(total)) + ' / ' + totalPct + '% / Total</td></tr>' +
          '</tfoot></table></div>';
        list.innerHTML = html;
        qs('spent-total').textContent = 'Total: ' + fmtDuration(total);
      }).catch(function () {
        qs('spent-total').textContent = 'Could not load time spent.';
        list.innerHTML = '<p class="settings-hint">Could not load time spent.</p>';
      });
    }

    function renderSpentRoles(rep, list) {
      var roles = (rep.roles || []).filter(function (r) { return r.seconds > 0; });
      var total = roles.reduce(function (sum, r) { return sum + r.seconds; }, 0);
      qs('spent-total').textContent = 'Total: ' + roleDuration(total);
      if (!roles.length) {
        list.innerHTML = '<p class="settings-hint">No entries exist for the given filter.</p>';
        return;
      }
      var html = '<div class="bshell-scroll"><table class="bshell-table spent-table spent-roles"><thead><tr>' +
        '<th>Role</th><th></th><th class="num">Duration</th></tr></thead><tbody>';
      roles.forEach(function (r) {
        var pct = total > 0 ? Math.round(r.seconds / total * 100) : 0;
        html += '<tr class="spent-row">' +
          '<td class="spent-role">' + escapeHtml(r.role || 'No role') + '</td>' +
          '<td class="spent-bar-cell"><div class="spent-bar-wrap"><div class="spent-bar-track">' +
            '<div class="spent-bar-fill" style="width:' + pct + '%"></div></div>' +
            '<span class="spent-bar-pct">' + pct + '%</span></div></td>' +
          '<td class="num">' + escapeHtml(roleDuration(r.seconds)) + '</td></tr>';
      });
      html += '</tbody><tfoot><tr class="spent-footer"><td>Total</td><td></td>' +
        '<td class="num">' + escapeHtml(roleDuration(total)) + '</td></tr></tfoot></table></div>';
      list.innerHTML = html;
    }

    function roleDuration(seconds) {
      return seconds < 60 ? seconds + 's' : fmtDuration(Math.floor(seconds / 60));
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
      qs('log-period-filter').addEventListener('change', periodChanged);
      ['log-custom-from', 'log-custom-to', 'log-relative-n', 'log-relative-unit',
       'log-board-filter', 'log-type-filter'].forEach(function (id) {
        qs(id).addEventListener('change', loadLog);
      });
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
      exportMenu.addEventListener('click', function (e) {
        var item = e.target.closest('[data-export]');
        if (!item) return;
        exportMenu.hidden = true;
        exportLog(item.getAttribute('data-export'));
      });
      // Dismiss back to the board: header ×, icon-row ×, backdrop click, Escape.
      qs('log-close-x').addEventListener('click', closeLogModal);
      qs('timer-log-overlay').addEventListener('click', function (e) {
        if (e.target === this) closeLogModal();
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

    // KF-393: Time spent filter bookmarks, kept in this browser.
    var SpentBookmarks = {
      key: 'pomb-spent-bookmarks',
      fields: ['spent-board', 'spent-period', 'spent-entry-type', 'spent-color', 'spent-group'],
      read: function () {
        try { return JSON.parse(window.localStorage.getItem(this.key) || '[]') || []; } catch (e) { return []; }
      },
      write: function (list) {
        try { window.localStorage.setItem(this.key, JSON.stringify(list)); } catch (e) { toast('Could not save the bookmark.'); }
        this.render();
      },
      render: function () {
        var self = this;
        var list = this.read();
        qs('spent-bookmarks-count').textContent = String(list.length);
        var ul = qs('spent-bookmarks-list');
        ul.innerHTML = '';
        list.forEach(function (b, i) {
          var li = document.createElement('li');
          var apply = document.createElement('button');
          apply.type = 'button';
          apply.className = 'spent-bookmark-apply';
          apply.textContent = b.name;
          apply.addEventListener('click', function () { self.apply(b); });
          var del = document.createElement('button');
          del.type = 'button';
          del.className = 'spent-bookmark-delete';
          del.setAttribute('aria-label', 'Delete bookmark ' + b.name);
          del.textContent = '×';
          del.addEventListener('click', function () {
            var next = self.read();
            next.splice(i, 1);
            self.write(next);
          });
          li.appendChild(apply);
          li.appendChild(del);
          ul.appendChild(li);
        });
      },
      save: function () {
        var input = qs('spent-bookmark-name');
        var name = input.value.trim();
        if (!name) { input.focus(); return; }
        var filters = {};
        this.fields.forEach(function (id) { filters[id] = qs(id).value; });
        var list = this.read();
        list.push({ name: name, filters: filters });
        input.value = '';
        this.write(list);
      },
      apply: function (bookmark) {
        this.fields.forEach(function (id) {
          if (bookmark.filters && id in bookmark.filters) qs(id).value = bookmark.filters[id];
        });
        loadSpent();
      },
      init: function () {
        var self = this;
        qs('spent-bookmarks-toggle').addEventListener('click', function () {
          var body = qs('spent-bookmarks-body');
          body.hidden = !body.hidden;
          qs('spent-bookmarks-toggle').setAttribute('aria-expanded', String(!body.hidden));
        });
        qs('spent-bookmark-save').addEventListener('click', function () { self.save(); });
        qs('spent-bookmark-name').addEventListener('keydown', function (e) {
          if (e.key === 'Enter') { e.preventDefault(); self.save(); }
        });
        this.render();
      },
    };

    function initSpentPage() {
      SpentBookmarks.init();
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
      // report (Pomb's filters are the always-visible row, KF-292).
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
          html += '</tbody></table></body></html>';
          var win = window.open('', '_blank');
          if (!win) { toast('Please allow pop-ups to export PDF.'); return; }
          win.document.write(html);
          win.document.close();
          win.onload = function () { win.print(); };
        }).catch(function () { toast('Export failed.'); });
      });
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

    // Escape on the timer log: the custom-range dialog, then the export
    // menu, then the modal.
    function escapeTop() {
      var cr = qs('log-custom-dialog-overlay');
      if (cr && !cr.hidden) { CustomRange.close(); return; }
      var menu = qs('log-export-menu');
      if (menu && !menu.hidden) { menu.hidden = true; return; }
      closeLogModal();
    }

    return {
      init: init, reload: loadLog, openModal: openLogModal, closeModal: closeLogModal, escape: escapeTop,
    };
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
        chart.innerHTML = '<p class="settings-hint">No data to display</p>';
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
        rh = '<p class="settings-hint">No data to display</p>';
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
        bh = '<p class="settings-hint">No break activities defined</p>';
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
      var hs = '<p class="settings-hint">No data to display</p>';
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
      qs('stats-chart').innerHTML = '<p class="settings-hint">Loading chart…</p>';
      fetchJson('/api/timer/statistics?' + p.toString()).then(function (rep) {
        render(rep);
      }).catch(function () {
        qs('stats-chart').innerHTML = '<p class="settings-hint">Could not load statistics.</p>';
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
      document.querySelectorAll('.bshell-nav-item[data-tab]').forEach(function (tab) {
        tab.addEventListener('click', function (e) {
          e.preventDefault();
          document.querySelectorAll('.bshell-nav-item[data-tab]').forEach(function (t) {
            t.classList.remove('bshell-nav-active');
          });
          tab.classList.add('bshell-nav-active');
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
    function exportColumns(tableFields) {
      return ['Number', 'Task', 'Description', 'Column', 'Swimlane', 'Color', 'Labels',
        'Members', 'Timeline start', 'Timeline end', 'Due date', 'Due done', 'Estimate (h)',
        'Points', 'Size (pomodori)',
        'Time spent (min)', 'Pomodoros', 'Interruptions', 'Created at',
        'Completed at', 'Added to column at', 'Subtasks', 'Comments']
        .concat(tableFields.map(function (def) { return def.label; }));
    }

    // Table fields are the only custom fields the export carries: one
    // column each in Excel and CSV, an array in JSON, rows in XML.
    function loadTableFields(boardId) {
      return fetch('/api/boards/' + encodeURIComponent(boardId) + '/fields',
        { headers: { 'Accept': 'application/json' } })
        .then(function (resp) { return resp.ok ? resp.json() : []; })
        .then(function (defs) {
          return defs.filter(function (def) { return def.kind === 'table'; });
        })
        .catch(function () { return []; });
    }

    function tableRows(t, def) {
      var rows = t.fields && t.fields[def.key];
      return Array.isArray(rows) ? rows : null;
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
      var record = {
        number: t.number || '',
        name: t.name || '',
        description: t.description || '',
        column: col ? col.name : '',
        swimlane: lane ? lane.name : '',
        color: color ? (color.label || color.value) : '',
        labels: (t.labels || []).join(', '),
        members: members.join(', '),
        timeline_start: t.timeline_start || '',
        timeline_end: t.timeline_end || '',
        due_at: t.due_at || '',
        due_done: t.due_done ? 'yes' : '',
        estimate_hours: t.estimate_hours == null ? '' : String(t.estimate_hours),
        points: t.points == null ? '' : String(t.points),
        size: t.size == null ? '' : String(t.size),
        total_minutes: String(t.total_minutes || 0),
        pomodori_completed: String(t.pomodori_completed || 0),
        interruptions: String(t.interruptions || 0),
        created_at: t.created_at || '',
        completed_at: t.completed_at || '',
        column_entered_at: t.column_entered_at || '',
        subtasks: doneSub + '/' + (t.subtasks || []).length,
        comments: String((t.comments || []).length),
      };
      if (lookups.tableFields.length) {
        record.fields = {};
        lookups.tableFields.forEach(function (def) {
          var rows = tableRows(t, def);
          if (rows) record.fields[def.key] = rows;
        });
      }
      return record;
    }

    function recordToRow(r, tableFields) {
      return [r.number, r.name, r.description, r.column, r.swimlane, r.color, r.labels,
        r.members, r.timeline_start, r.timeline_end, r.due_at, r.due_done, r.estimate_hours, r.points, r.size,
        r.total_minutes, r.pomodori_completed, r.interruptions, r.created_at,
        r.completed_at, r.column_entered_at, r.subtasks, r.comments]
        .concat(tableFields.map(function (def) {
          var rows = r.fields && r.fields[def.key];
          return rows ? JSON.stringify(rows) : '';
        }));
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

    function downloadCsv(filename, rows) {
      var out = rows.map(function (r) { return r.map(csvCell).join(','); }).join('\r\n');
      download(filename, '﻿' + out, 'text/csv;charset=utf-8');
    }

    function xmlEscape(s) {
      return String(s == null ? '' : s)
        .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;').replace(/'/g, '&apos;');
    }

    function xmlTag(name) { return String(name).replace(/[^A-Za-z0-9_]/g, '_'); }

    // One element per row, with one child per cell that is set
    function fieldsXml(fields) {
      var out = '    <fields>\n';
      Object.keys(fields).forEach(function (key) {
        var tag = xmlTag(key);
        out += '      <' + tag + '>\n';
        fields[key].forEach(function (row) {
          out += '        <row>\n';
          Object.keys(row).forEach(function (col) {
            var cell = Array.isArray(row[col]) ? row[col].join(', ') : row[col];
            var cellTag = xmlTag(col);
            out += '          <' + cellTag + '>' + xmlEscape(cell) + '</' + cellTag + '>\n';
          });
          out += '        </row>\n';
        });
        out += '      </' + tag + '>\n';
      });
      return out + '    </fields>\n';
    }

    function downloadXml(filename, records, boardName) {
      var xml = '<?xml version="1.0" encoding="UTF-8"?>\n<tasks board="' +
        xmlEscape(boardName) + '">\n';
      records.forEach(function (r) {
        xml += '  <task>\n';
        Object.keys(r).forEach(function (k) {
          var tag = xmlTag(k);
          if (k === 'fields') {
            xml += fieldsXml(r.fields);
            return;
          }
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
      Promise.all([
        fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data',
          { headers: { 'Accept': 'application/json' } })
          .then(function (resp) {
            if (!resp.ok) throw new Error('HTTP ' + resp.status);
            return resp.json();
          }),
        loadTableFields(boardId),
      ])
        .then(function (loaded) {
          var data = loaded[0];
          var tableFields = loaded[1];
          var lookups = {
            tableFields: tableFields,
            columns: lookupMap(data.columns),
            swimlanes: lookupMap(data.swimlanes),
            colors: lookupMap(data.colors),
            users: lookupMap(data.users),
          };
          var records = (data.tasks || []).map(function (t) { return taskRecord(t, lookups); });
          var base = sanitizeFilename(data.board && data.board.name) + '-export';
          var rows;
          if (format === 'excel') {
            rows = [exportColumns(tableFields)];
            records.forEach(function (r) { rows.push(recordToRow(r, tableFields)); });
            downloadExcel(base + '.xls', rows);
          } else if (format === 'csv') {
            rows = [exportColumns(tableFields)];
            records.forEach(function (r) { rows.push(recordToRow(r, tableFields)); });
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
      var buttons = document.querySelectorAll('[data-export-format]');
      buttons.forEach(function (btn) {
        btn.addEventListener('click', function () {
          doExport(boardId, btn.getAttribute('data-export-format'));
        });
      });
    }

    return { init: init };
  })();

  var ReportBreakdown = (function () {
    var GROUPS = { total: 'Total', color: 'Color', label: 'Label', responsible: 'Responsible', user: 'User' };

    var NONE = 'none';
    var DEFAULT_RANGE_DAYS = 6;

    function parseDay(str) {
      if (!/^\d{4}-\d{2}-\d{2}$/.test(str || '')) return null;
      var parts = str.split('-');
      return new Date(+parts[0], +parts[1] - 1, +parts[2]).getTime();
    }

    function periodRange(value, fromStr, toStr, now) {
      var y = now.getFullYear(), m = now.getMonth();
      if (value === 'this-month') return { from: new Date(y, m, 1).getTime(), to: new Date(y, m + 1, 1).getTime() };
      if (value === 'last-month') return { from: new Date(y, m - 1, 1).getTime(), to: new Date(y, m, 1).getTime() };
      if (value === 'custom') {
        var a = parseDay(fromStr), b = parseDay(toStr);
        if (a === null || b === null || b < a) return null;
        var after = new Date(b);
        after.setDate(after.getDate() + 1);
        return { from: a, to: after.getTime() };
      }
      var to = new Date(y, m, now.getDate() + 1);
      return { from: new Date(y, m, now.getDate() + 1 - parseInt(value, 10)).getTime(), to: to.getTime() };
    }

    function groupsOf(t, mode, ctx) {
      if (mode === 'total') return [{ key: 'total', label: 'Total', dot: null }];
      if (mode === 'color') {
        var c = ctx.colorById[t.color_id];
        return [{ key: t.color_id || '', label: c ? (c.label || c.value) : 'No color', dot: c ? c.value : null }];
      }
      if (mode === 'label') {
        var labels = t.labels || [];
        return labels.length ? labels.map(function (l) { return { key: l, label: l, dot: null }; })
          : [{ key: '', label: 'No label', dot: null }];
      }
      if (mode === 'responsible') {
        var r = t.responsible;
        if (r && r.kind === 'member') return [{ key: 'u:' + r.id, label: ctx.userName(r.id), dot: null }];
        if (r && r.kind === 'session') return [{ key: 's:' + r.name, label: r.name, dot: null }];
        return [{ key: '', label: 'Unassigned', dot: null }];
      }
      var ids = t.member_ids || [];
      return ids.length ? ids.map(function (id) { return { key: id, label: ctx.userName(id), dot: null }; })
        : [{ key: '', label: 'Unassigned', dot: null }];
    }

    function matches(t, f) {
      var members = t.member_ids || [];
      if (f.user === NONE) {
        if (members.length) return false;
      } else if (f.user && members.indexOf(f.user) === -1) {
        return false;
      }
      if (f.color === NONE) {
        if (t.color_id) return false;
      } else if (f.color && t.color_id !== f.color) {
        return false;
      }
      return !f.label || (t.labels || []).indexOf(f.label) !== -1;
    }

    function passes(t, opts) {
      if (t.completed_at && opts.range) {
        var at = Date.parse(t.completed_at);
        if (!isNaN(at) && (at < opts.range.from || at >= opts.range.to)) return false;
      }
      return matches(t, opts);
    }

    function build(data, opts, valueOf) {
      var columns = data.columns || [];
      var colIndex = {};
      columns.forEach(function (c, i) { colIndex[c.id] = i; });
      var colorById = {}, userById = {};
      (data.colors || []).forEach(function (c) { colorById[c.id] = c; });
      (data.users || []).forEach(function (u) { userById[u.id] = u.username; });
      var ctx = { colorById: colorById, userName: function (id) { return userById[id] || 'Unknown user'; } };
      var rows = [], byKey = Object.create(null);
      function rowFor(g) {
        if (!byKey[g.key]) {
          byKey[g.key] = { label: g.label, value: g.dot, sums: columns.map(function () { return 0; }) };
          rows.push(byKey[g.key]);
        }
        return byKey[g.key];
      }
      if (opts.group === 'total') rowFor({ key: 'total', label: 'Total', dot: null });
      var matched = 0, total = 0;
      (data.tasks || []).forEach(function (t) {
        var i = colIndex[t.column_id];
        if (i === undefined || !passes(t, opts)) return;
        var v = valueOf(t);
        if (v > 0) matched++;
        total += v;
        groupsOf(t, opts.group, ctx).forEach(function (g) { rowFor(g).sums[i] += v; });
      });
      return { columns: columns, rows: rows, matched: matched, total: total, group: opts.group };
    }

    function fill(select, items) {
      items.forEach(function (it) {
        var opt = document.createElement('option');
        opt.value = it.value;
        opt.textContent = it.text;
        select.appendChild(opt);
      });
    }

    function fillFilters(data, prefix) {
      var labels = Object.create(null);
      (data.tasks || []).forEach(function (t) {
        (t.labels || []).forEach(function (l) { labels[l] = true; });
      });
      fill(document.getElementById(prefix + 'user'), [{ value: NONE, text: 'Unassigned' }].concat(
        (data.users || []).map(function (u) { return { value: u.id, text: u.username }; })));
      fill(document.getElementById(prefix + 'color'), [{ value: NONE, text: 'No color' }].concat(
        (data.colors || []).map(function (c) { return { value: c.id, text: c.label || c.value }; })));
      fill(document.getElementById(prefix + 'label'), Object.keys(labels).sort().map(function (l) {
        return { value: l, text: l };
      }));
    }

    function mount(boardId, handlers) {
      function qs(id) { return document.getElementById(id); }
      var data = null;
      function options() {
        return {
          group: qs('rb-group').value,
          range: periodRange(qs('rb-period').value, qs('rb-from').value, qs('rb-to').value, new Date()),
          user: qs('rb-user').value,
          color: qs('rb-color').value,
          label: qs('rb-label').value,
        };
      }
      function refresh() {
        qs('rb-custom').hidden = qs('rb-period').value !== 'custom';
        if (data) handlers.render(data, options());
      }
      var today = new Date();
      var from = new Date(today.getFullYear(), today.getMonth(), today.getDate() - DEFAULT_RANGE_DAYS);
      qs('rb-from').value = isoDate(from);
      qs('rb-to').value = isoDate(today);
      ['rb-group', 'rb-period', 'rb-from', 'rb-to', 'rb-user', 'rb-color', 'rb-label'].forEach(function (id) {
        qs(id).addEventListener('change', refresh);
      });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' },
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (loaded) {
        data = loaded;
        fillFilters(data, 'rb-');
        refresh();
      }).catch(function (err) {
        handlers.error(err);
      });
    }

    function tableHtml(table, cellText) {
      if (!table.rows.length) return '<p class="settings-hint">No tasks match.</p>';
      var html = '<table class="bshell-table"><thead><tr><th scope="col"><span class="sr-only">' +
        escapeHtml(GROUPS[table.group]) + '</span></th>';
      table.columns.forEach(function (c) {
        html += '<th scope="col" class="num">' + escapeHtml(c.name) + '</th>';
      });
      html += '</tr></thead><tbody>';
      table.rows.forEach(function (r) {
        var dot = table.group !== 'color' ? '' : '<span class="report-color-dot"' +
          (r.value
            ? ' style="background:' + escapeHtml(r.value) + '"'
            : ' style="background:transparent;border:1px dashed #9ca3af"') + '></span> ';
        html += '<tr><th scope="row">' + dot + escapeHtml(r.label) + '</th>';
        r.sums.forEach(function (v) {
          html += '<td class="num">' + escapeHtml(String(cellText(v))) + '</td>';
        });
        html += '</tr>';
      });
      return html + '</tbody></table>';
    }

    return {
      build: build, mount: mount, tableHtml: tableHtml, periodRange: periodRange, matches: matches,
      fill: fill, fillFilters: fillFilters, parseDay: parseDay, GROUPS: GROUPS, NONE: NONE,
    };
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

    function render(data, opts) {
      if (!opts.range) {
        setStatus('Pick a start date that is on or before the end date.');
        return;
      }
      var table = ReportBreakdown.build(data, opts, function () { return 1; });
      qs('taskcount-table').innerHTML = ReportBreakdown.tableHtml(table, function (n) { return n; });
      var total = table.matched;
      setStatus(total === 1 ? '1 task.' : total + ' tasks.');
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      ReportBreakdown.mount(boardId, {
        render: render,
        error: function (err) { setStatus('Could not load task data: ' + err.message); },
      });
    }

    return { init: init };
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
      return d.getDate() + ' ' + LONG_MONTHS[d.getMonth()] + ' ' + clockText(d);
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

  // Throughput report page. Counts the tasks that arrive in a target
  // column, or their time estimate, per day, week, several weeks or month.
  var ThroughputPage = (function () {
    var initialized = false;
    var GEO = { W: 720, H: 300, padL: 46, padR: 32, padT: 16, padB: 34 };
    var PERIOD = {
      WEEKS_12: '12w', WEEKS_24: '24w', THIS_YEAR: 'this-year', LAST_YEAR: 'last-year',
      ABSOLUTE: 'absolute', RELATIVE: 'relative',
    };
    var WEEKS_12 = 12;
    var WEEKS_24 = 24;
    var DEFAULT_DAYS_BACK = 84;
    var DEFAULT_VALUE_TYPE = 'count';
    var DEFAULT_GROUP = 'week';
    var MONTH = 'month';
    var DAYS_PER_WEEK = 7;
    var MAX_TICKS = 12;
    var HOLDER = { HUMAN: 'human', AGENT: 'agent' };
    var VALUE_TYPES = ['count', 'estimate'];
    var GROUPS = {
      day: { text: 'Day', weeks: 0 },
      week: { text: 'Week', weeks: 1 },
      '2w': { text: '2 weeks', weeks: 2 },
      '3w': { text: '3 weeks', weeks: 3 },
      '4w': { text: '4 weeks', weeks: 4 },
      month: { text: 'Month', weeks: 0 },
    };
    var UNASSIGNED = 'unassigned';
    var OWNER_PREFIX_HUMAN = 'u:';
    var OWNER_PREFIX_AGENT = 'a:';

    function qs(id) { return document.getElementById(id); }

    function roundTenth(n) { return ReportRange.roundTenth(n); }

    function setStatus(msg) {
      var el = qs('throughput-status');
      if (el) el.textContent = msg;
    }

    function weekStart(d) {
      var x = new Date(d.getFullYear(), d.getMonth(), d.getDate());
      x.setDate(x.getDate() - daysIntoWeek(x));
      return x;
    }

    function daysAfter(d, n) {
      return new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);
    }

    function periodRange(values, now) {
      var today = ReportChart.dayStart(now);
      var y = today.getFullYear();
      var range;
      var n;
      if (values.period === PERIOD.THIS_YEAR) {
        range = { start: new Date(y, 0, 1), end: today };
      } else if (values.period === PERIOD.LAST_YEAR) {
        range = { start: new Date(y - 1, 0, 1), end: new Date(y - 1, 11, 31) };
      } else if (values.period === PERIOD.ABSOLUTE) {
        var a = ReportBreakdown.parseDay(values.from), b = ReportBreakdown.parseDay(values.to);
        if (a === null || b === null || b < a) return null;
        range = { start: new Date(a), end: new Date(Math.min(b, today.getTime())) };
        if (range.end < range.start) return null;
      } else if (values.period === PERIOD.RELATIVE) {
        n = parseInt(values.days, 10);
        if (!(n >= 1)) return null;
        range = { start: daysAfter(today, 1 - n), end: today };
      } else {
        n = values.period === PERIOD.WEEKS_24 ? WEEKS_24 : WEEKS_12;
        range = { start: daysAfter(weekStart(today), -DAYS_PER_WEEK * (n - 1)), end: today };
      }
      if ((range.end - range.start) / ReportRange.DAY_MS > ReportRange.MAX_DAYS) return null;
      return range;
    }

    function nextStart(start, group) {
      if (group === MONTH) return new Date(start.getFullYear(), start.getMonth() + 1, 1);
      return daysAfter(start, GROUPS[group].weeks ? DAYS_PER_WEEK * GROUPS[group].weeks : 1);
    }

    function firstStart(range, group) {
      if (group === MONTH) return new Date(range.start.getFullYear(), range.start.getMonth(), 1);
      return GROUPS[group].weeks ? weekStart(range.start) : range.start;
    }

    function bucketValues(arrivals, range, group) {
      var starts = [];
      var at = firstStart(range, group);
      while (at.getTime() <= range.end.getTime()) {
        starts.push(at);
        at = nextStart(at, group);
      }
      var values = starts.map(function () { return 0; });
      var from = range.start.getTime();
      var to = daysAfter(range.end, 1).getTime();
      arrivals.forEach(function (a) {
        if (a.at < from || a.at >= to) return;
        var i = starts.length - 1;
        while (i > 0 && starts[i].getTime() > a.at) i--;
        values[i] += a.value;
      });
      return { starts: starts, values: values };
    }

    function arrivals(data, tasks, targetId, valueOf) {
      var cols = ColumnHistory.lookup(data);
      var out = [];
      tasks.forEach(function (t) {
        var tl = ColumnHistory.timelineFor(t, cols);
        var value = valueOf(t);
        tl.forEach(function (step, i) {
          if (step.col === targetId && (i === 0 || tl[i - 1].col !== targetId)) {
            out.push({ at: step.at, value: value });
          }
        });
      });
      return out;
    }

    function ownerMatches(task, chosen) {
      if (!chosen) return true;
      var holder = task.owner && task.owner.holder;
      if (chosen === UNASSIGNED) return !holder;
      if (!holder) return false;
      if (holder.kind === HOLDER.HUMAN) return chosen === OWNER_PREFIX_HUMAN + holder.id;
      return chosen === OWNER_PREFIX_AGENT + holder.name;
    }

    function selectTasks(data, values) {
      var rest = { user: '', color: values.color, labels: values.labels, labelMode: values.labelMode };
      return ReportFilter.filterTasks(data.tasks || [], rest).filter(function (t) {
        return ownerMatches(t, values.user);
      });
    }

    function series(data, values, now) {
      var range = periodRange(values, now);
      if (!range) return null;
      var type = ReportValue[values.valueType] ? values.valueType : DEFAULT_VALUE_TYPE;
      var group = GROUPS[values.group] ? values.group : DEFAULT_GROUP;
      var tasks = selectTasks(data, values);
      var buckets = bucketValues(
        arrivals(data, tasks, values.target, ReportValue[type].valueOf), range, group);
      return {
        starts: buckets.starts, values: buckets.values, type: type, group: group, matched: tasks.length,
      };
    }

    function unitText(type, n) {
      var unit = ReportValue[type].unit;
      return roundTenth(n) + ' ' + (n === 1 ? unit.replace(/s$/, '') : unit);
    }

    function chartSvg(s) {
      var W = GEO.W, H = GEO.H, padL = GEO.padL, padR = GEO.padR, padT = GEO.padT, padB = GEO.padB;
      var innerW = W - padL - padR, innerH = H - padT - padB;
      var max = 1;
      s.values.forEach(function (v) { if (v > max) max = v; });
      max = Math.ceil(max / 5) * 5;

      var n = s.values.length;
      function x(i) { return ReportChart.pointX(GEO, n, i); }
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
        ' transform="rotate(-90 14 ' + midY + ')">' + escapeHtml(ReportValue[s.type].axis) + '</text>';
      var step = Math.max(1, Math.ceil(n / MAX_TICKS));
      var i, j, k;
      for (i = 0; i < n; i += step) {
        svg += '<text x="' + x(i).toFixed(1) + '" y="' + (H - 10) +
          '" text-anchor="middle" class="chart-tick">' +
          escapeHtml(ReportChart.shortDate(s.starts[i])) + '</text>';
      }
      var pts = [];
      for (j = 0; j < n; j++) {
        pts.push(x(j).toFixed(1) + ',' + y(s.values[j]).toFixed(1));
      }
      svg += '<polyline points="' + pts.join(' ') + '" class="chart-line" fill="none"/>';
      for (k = 0; k < n; k++) {
        svg += '<circle cx="' + x(k).toFixed(1) + '" cy="' + y(s.values[k]).toFixed(1) +
          '" r="4" class="chart-dot"/>';
      }
      return svg + ReportChart.crosshairLine(GEO) + '</svg>';
    }

    function tipFor(s, i) {
      return {
        title: ReportChart.longDate(s.starts[i]),
        rows: [{ label: ReportValue[s.type].text, value: unitText(s.type, s.values[i]) }],
      };
    }

    var current = null;

    function renderChart(s) {
      current = s;
      var host = qs('throughput-chart');
      if (!host) return;
      host.innerHTML = chartSvg(s);
      ReportChart.attachCrosshair(host, GEO, s.values.length, function (i) { return tipFor(s, i); });
    }

    function rows() {
      if (!current) return [];
      return [[GROUPS[current.group].text + ' starting', ReportValue[current.type].text]].concat(
        current.starts.map(function (start, i) { return [isoDate(start), roundTenth(current.values[i])]; }));
    }

    function ownerControl(data) {
      var agents = Object.create(null);
      (data.tasks || []).forEach(function (t) {
        var holder = t.owner && t.owner.holder;
        if (holder && holder.kind === HOLDER.AGENT) agents[holder.name] = true;
      });
      return {
        id: 'user', label: 'User', value: '',
        options: [{ value: '', text: 'Show all' }, { value: UNASSIGNED, text: 'Unassigned' }]
          .concat((data.users || []).map(function (u) { return { value: OWNER_PREFIX_HUMAN + u.id, text: u.username }; }))
          .concat(Object.keys(agents).sort().map(function (name) { return { value: OWNER_PREFIX_AGENT + name, text: name }; })),
      };
    }

    function defaultTarget(columns) {
      var done = columns.filter(function (c) { return c.is_done; });
      var pick = done.length ? done[0] : columns[columns.length - 1];
      return pick ? pick.id : '';
    }

    function controls(data, now) {
      var columns = data.columns || [];
      var today = ReportChart.dayStart(now);
      return [{
        id: 'valueType', label: 'Value type', value: DEFAULT_VALUE_TYPE,
        options: VALUE_TYPES.map(function (key) { return { value: key, text: ReportValue[key].text }; }),
      }, {
        id: 'target', label: 'Target column', value: defaultTarget(columns),
        options: columns.map(function (c) { return { value: c.id, text: c.name }; }),
      }, {
        id: 'period', label: 'Period', value: PERIOD.WEEKS_12,
        options: [
          { value: PERIOD.WEEKS_12, text: 'Last 12 weeks' },
          { value: PERIOD.WEEKS_24, text: 'Last 24 weeks' },
          { value: PERIOD.THIS_YEAR, text: 'This year' },
          { value: PERIOD.LAST_YEAR, text: 'Last year' },
          { value: PERIOD.ABSOLUTE, text: 'Custom absolute' },
          { value: PERIOD.RELATIVE, text: 'Custom relative' },
        ],
      }, {
        id: 'from', label: 'From', type: 'date', value: isoDate(daysAfter(today, 1 - DEFAULT_DAYS_BACK)),
        when: { id: 'period', value: PERIOD.ABSOLUTE },
      }, {
        id: 'to', label: 'To', type: 'date', value: isoDate(today),
        when: { id: 'period', value: PERIOD.ABSOLUTE },
      }, {
        id: 'days', label: 'Days back', type: 'number', max: ReportRange.MAX_DAYS, value: String(DEFAULT_DAYS_BACK),
        when: { id: 'period', value: PERIOD.RELATIVE },
      }, ownerControl(data), ReportFilter.colorControl(data)]
        .concat(ReportFilter.labelControls(data))
        .concat([{
          id: 'group', label: 'Group by', value: DEFAULT_GROUP,
          options: Object.keys(GROUPS).map(function (key) { return { value: key, text: GROUPS[key].text }; }),
        }]);
    }

    function render(data, values) {
      var s = series(data, values, new Date());
      if (!s) {
        current = null;
        var blank = qs('throughput-chart');
        if (blank) blank.innerHTML = '';
        setStatus('Pick a valid period of at most ' + ReportRange.MAX_YEARS + ' years that starts on or before today.');
        return;
      }
      renderChart(s);
      var total = s.values.reduce(function (a, b) { return a + b; }, 0);
      if ((data.tasks || []).length === 0) setStatus('No tasks on this board.');
      else if (s.matched === 0) setStatus('No tasks match the filter.');
      else setStatus(unitText(s.type, total) + ' reached the target column in the period.');
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' },
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        var state = ReportFilter.open(qs('report-filter'), controls(data, new Date()), function (values) {
          render(data, values);
        }, function (err) {
          setStatus('Could not draw the chart: ' + err.message);
        });
        ReportTools.mount({
          report: 'throughput', title: 'Throughput', boardId: boardId, chartId: 'throughput-chart',
          getFilters: state.applied, applyFilters: state.apply, rows: rows,
        });
      }).catch(function (err) {
        setStatus('Could not load task data: ' + err.message);
      });
    }

    return {
      init: init, chartSvg: chartSvg, tipFor: tipFor, series: series, periodRange: periodRange,
      bucketValues: bucketValues, arrivals: arrivals, controls: controls, render: render,
    };
  })();

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

  var ReportTools = (function () {
    var SVG_NS = 'http://www.w3.org/2000/svg';
    var PNG_SCALE = 2;
    var NAME_MAX = 60;
    var STYLE_PROPS = [
      'fill', 'fill-opacity', 'stroke', 'stroke-width', 'stroke-opacity', 'stroke-dasharray',
      'stroke-linecap', 'stroke-linejoin', 'opacity', 'font-family', 'font-size', 'font-weight',
      'text-anchor', 'dominant-baseline',
    ];
    var EXPORTS = [
      { kind: 'png', label: 'PNG image' },
      { kind: 'svg', label: 'SVG image' },
      { kind: 'xlsx', label: 'Excel' },
    ];

    function endpoint(boardId, id) {
      return '/api/boards/' + encodeURIComponent(boardId) + '/report-bookmarks' +
        (id ? '/' + encodeURIComponent(id) : '');
    }

    function pagePath(boardId, report) {
      return '/b/' + encodeURIComponent(boardId) + '/' + report;
    }

    function bookmarkPath(boardId, report, id) {
      return pagePath(boardId, report) + '?bookmark=' + encodeURIComponent(id);
    }

    function bookmarkIdOf(search) {
      var match = /[?&]bookmark=([^&]*)/.exec(search || '');
      if (!match) return '';
      try { return decodeURIComponent(match[1]); } catch (e) { return ''; }
    }

    function send(method, url, body) {
      var init = { method: method, headers: { 'Accept': 'application/json' }, credentials: 'same-origin' };
      if (body !== undefined) {
        init.headers['Content-Type'] = 'application/json';
        init.body = JSON.stringify(body);
      }
      return fetch(url, init).then(function (r) {
        if (r.ok) return method === 'DELETE' ? null : r.json();
        return r.text().then(function (text) {
          var message = text;
          try { message = JSON.parse(text).message || text; } catch (e) {}
          throw new Error(message || ('HTTP ' + r.status));
        });
      });
    }

    function standaloneSvg(svg) {
      var clone = svg.cloneNode(true);
      var from = svg.querySelectorAll('*');
      var to = clone.querySelectorAll('*');
      Array.prototype.forEach.call(from, function (el, i) {
        var computed = window.getComputedStyle(el);
        STYLE_PROPS.forEach(function (prop) {
          var value = computed.getPropertyValue(prop);
          if (value) to[i].style.setProperty(prop, value);
        });
      });
      Array.prototype.forEach.call(clone.querySelectorAll('.chart-crosshair'), function (el) {
        el.parentNode.removeChild(el);
      });
      var box = svg.viewBox.baseVal;
      var back = document.createElementNS(SVG_NS, 'rect');
      back.setAttribute('width', String(box.width));
      back.setAttribute('height', String(box.height));
      back.setAttribute('fill', '#ffffff');
      clone.insertBefore(back, clone.firstChild);
      clone.setAttribute('xmlns', SVG_NS);
      clone.setAttribute('width', String(box.width));
      clone.setAttribute('height', String(box.height));
      return { text: new XMLSerializer().serializeToString(clone), width: box.width, height: box.height };
    }

    function svgToPng(image) {
      return new Promise(function (resolve, reject) {
        var img = new Image();
        img.onload = function () {
          var canvas = document.createElement('canvas');
          canvas.width = image.width * PNG_SCALE;
          canvas.height = image.height * PNG_SCALE;
          canvas.getContext('2d').drawImage(img, 0, 0, canvas.width, canvas.height);
          canvas.toBlob(function (blob) {
            if (blob) resolve(blob); else reject(new Error('The browser could not draw the image.'));
          }, 'image/png');
        };
        img.onerror = function () { reject(new Error('The browser could not draw the image.')); };
        img.src = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(image.text);
      });
    }

    function exportChart(config, kind) {
      var host = document.getElementById(config.chartId);
      var svg = host ? host.querySelector('svg') : null;
      if (!svg) return Promise.reject(new Error('There is no chart to export yet.'));
      if (kind === 'xlsx') {
        var rows = config.rows();
        if (!rows || !rows.length) return Promise.reject(new Error('There is no data to export yet.'));
        saveBlob(config.report + '.xlsx',
          new Blob([ReportFile.xlsxBytes(rows, config.title)], { type: ReportFile.XLSX_MIME }));
        return Promise.resolve();
      }
      var image = standaloneSvg(svg);
      if (kind === 'svg') {
        saveBlob(config.report + '.svg', new Blob([image.text], { type: 'image/svg+xml;charset=utf-8' }));
        return Promise.resolve();
      }
      return svgToPng(image).then(function (blob) { saveBlob(config.report + '.png', blob); });
    }

    function tag(text) { return '<span class="rt-tag">' + text + '</span>'; }

    function rowHtml(b) {
      var html = '<li data-id="' + escapeHtml(b.id) + '"><button type="button" class="rt-apply">' +
        escapeHtml(b.name) + '</button>';
      if (b.shared) html += tag('Shared');
      if (b.shortcut) html += tag('Shortcut');
      html += '<button type="button" class="rt-more" aria-haspopup="menu" aria-expanded="false" ' +
        'aria-label="Menu for ' + escapeHtml(b.name) + '">&hellip;</button>' +
        '<div class="menu-pop rt-row-menu" role="menu" hidden>';
      if (b.own) {
        html += '<button type="button" role="menuitem" data-act="edit">Edit</button>' +
          '<button type="button" role="menuitem" data-act="update">Update</button>';
      }
      html += '<button type="button" role="menuitem" data-act="url">Get URL</button>';
      if (b.own) html += '<button type="button" role="menuitem" data-act="delete">Delete</button>';
      return html + '</div></li>';
    }

    function mount(config) {
      var host = document.getElementById('report-tools');
      if (!host) return;
      var list = [];
      var editing = '';
      host.innerHTML =
        '<div class="rt-bar">' +
        '<button type="button" class="kf-btn" data-rt="toggle" aria-expanded="false">Bookmarks (<span data-rt="count">0</span>)</button>' +
        '<span class="rt-export"><button type="button" class="kf-btn" data-rt="export" aria-haspopup="menu" aria-expanded="false">Export</button>' +
        '<div class="menu-pop rt-export-menu" role="menu" hidden>' +
        EXPORTS.map(function (e) {
          return '<button type="button" role="menuitem" data-export="' + e.kind + '">' + e.label + '</button>';
        }).join('') + '</div></span></div>' +
        '<div class="rt-drawer settings-form" data-rt="drawer" hidden>' +
        '<ul class="rt-list" data-rt="list"></ul>' +
        '<form class="rt-form" data-rt="form">' +
        '<input type="text" data-rt="name" maxlength="' + NAME_MAX + '" placeholder="Bookmark name" aria-label="Bookmark name" autocomplete="off">' +
        '<label class="check-label"><input type="checkbox" data-rt="shared"> Share with board members</label>' +
        '<label class="check-label"><input type="checkbox" data-rt="shortcut"> Show shortcut in report list</label>' +
        '<div class="rt-form-actions"><button type="submit" class="kf-btn kf-btn-primary" data-rt="submit">Add bookmark</button>' +
        '<button type="button" class="kf-btn" data-rt="cancel" hidden>Cancel</button></div></form>' +
        '<p class="rt-status settings-hint" data-rt="status" role="status"></p></div>';

      function part(name) { return host.querySelector('[data-rt="' + name + '"]'); }
      var drawer = part('drawer');
      var status = part('status');
      var exportMenu = host.querySelector('.rt-export-menu');

      function say(message) { status.textContent = message; }
      function fail(err) { say(err.message); }

      function closeMenus() {
        exportMenu.hidden = true;
        part('export').setAttribute('aria-expanded', 'false');
        Array.prototype.forEach.call(host.querySelectorAll('.rt-row-menu'), function (m) { m.hidden = true; });
      }

      function render() {
        part('count').textContent = String(list.length);
        part('list').innerHTML = list.length ? list.map(rowHtml).join('') : '<li class="rt-empty">No bookmarks yet.</li>';
      }

      function find(id) {
        return list.filter(function (b) { return b.id === id; })[0];
      }

      function resetForm() {
        editing = '';
        part('name').value = '';
        part('shared').checked = false;
        part('shortcut').checked = false;
        part('submit').textContent = 'Add bookmark';
        part('cancel').hidden = true;
      }

      function applyBookmark(b) {
        config.applyFilters(b.filters || {});
        say('Applied "' + b.name + '".');
      }

      function replace(saved) {
        list = list.map(function (b) { return b.id === saved.id ? saved : b; });
        render();
      }

      function submit() {
        var name = part('name').value.trim();
        if (!name) { say('Give the bookmark a name.'); return; }
        var flags = { name: name, shared: part('shared').checked, shortcut: part('shortcut').checked };
        if (editing) {
          send('PATCH', endpoint(config.boardId, editing), flags).then(function (saved) {
            replace(saved);
            resetForm();
            say('Saved "' + saved.name + '".');
          }).catch(fail);
          return;
        }
        flags.report = config.report;
        flags.filters = config.getFilters();
        send('POST', endpoint(config.boardId), flags).then(function (saved) {
          list.push(saved);
          render();
          resetForm();
          say('Added "' + saved.name + '".');
        }).catch(fail);
      }

      function showUrl(b) {
        var url = window.location.origin + bookmarkPath(config.boardId, config.report, b.id);
        Array.prototype.forEach.call(host.querySelectorAll('.rt-url'), function (old) { old.remove(); });
        part('list').insertAdjacentHTML('afterend',
          '<input type="text" class="rt-url" readonly aria-label="Bookmark URL" value="' + escapeHtml(url) + '">');
        var field = host.querySelector('.rt-url');
        field.select();
        var copied = window.navigator.clipboard && window.navigator.clipboard.writeText
          ? window.navigator.clipboard.writeText(url) : Promise.reject(new Error('no clipboard'));
        copied.then(function () { say('Copied the URL of "' + b.name + '".'); },
          function () { say('Copy the URL of "' + b.name + '" from the box.'); });
        field.addEventListener('blur', function () { if (field.parentNode) field.parentNode.removeChild(field); });
      }

      function rowAction(act, b, li) {
        if (act === 'edit') {
          editing = b.id;
          part('name').value = b.name;
          part('shared').checked = b.shared;
          part('shortcut').checked = b.shortcut;
          part('submit').textContent = 'Save changes';
          part('cancel').hidden = false;
          part('name').focus();
        } else if (act === 'update') {
          send('PATCH', endpoint(config.boardId, b.id), { filters: config.getFilters() }).then(function (saved) {
            replace(saved);
            say('Updated "' + saved.name + '" with the current filters.');
          }).catch(fail);
        } else if (act === 'url') {
          showUrl(b);
        } else if (act === 'delete') {
          rowAction('keep', b, li);
          li.insertAdjacentHTML('beforeend',
            '<span class="rt-confirm">Delete "' + escapeHtml(b.name) + '"? ' +
            '<button type="button" class="kf-btn kf-btn-danger" data-act="confirm-delete">Delete</button>' +
            '<button type="button" class="kf-btn" data-act="keep">Keep</button></span>');
        } else if (act === 'confirm-delete') {
          send('DELETE', endpoint(config.boardId, b.id)).then(function () {
            list = list.filter(function (x) { return x.id !== b.id; });
            if (editing === b.id) resetForm();
            render();
            say('Deleted "' + b.name + '".');
          }).catch(fail);
        } else if (act === 'keep') {
          var box = li.querySelector('.rt-confirm');
          if (box) box.parentNode.removeChild(box);
        }
      }

      host.addEventListener('click', function (e) {
        var t = e.target.closest ? e.target.closest('button') : null;
        if (!t) return;
        var kind = t.getAttribute('data-export');
        var li = t.closest ? t.closest('li[data-id]') : null;
        if (t.getAttribute('data-rt') === 'toggle') {
          drawer.hidden = !drawer.hidden;
          t.setAttribute('aria-expanded', String(!drawer.hidden));
        } else if (t.getAttribute('data-rt') === 'export') {
          var open = exportMenu.hidden;
          closeMenus();
          exportMenu.hidden = !open;
          t.setAttribute('aria-expanded', String(open));
        } else if (kind) {
          closeMenus();
          exportChart(config, kind).catch(function (err) { toast(err.message); });
        } else if (t.getAttribute('data-rt') === 'cancel') {
          resetForm();
        } else if (li && t.classList.contains('rt-apply')) {
          applyBookmark(find(li.getAttribute('data-id')));
        } else if (li && t.classList.contains('rt-more')) {
          var menu = li.querySelector('.rt-row-menu');
          var show = menu.hidden;
          closeMenus();
          menu.hidden = !show;
          t.setAttribute('aria-expanded', String(show));
        } else if (li && t.getAttribute('data-act')) {
          closeMenus();
          rowAction(t.getAttribute('data-act'), find(li.getAttribute('data-id')), li);
        }
      });
      host.addEventListener('submit', function (e) {
        e.preventDefault();
        submit();
      });
      document.addEventListener('click', function (e) {
        if (!host.contains(e.target)) closeMenus();
      });

      send('GET', endpoint(config.boardId)).then(function (data) {
        list = (data.bookmarks || []).filter(function (b) { return b.report === config.report; });
        render();
        var wanted = bookmarkIdOf(window.location.search);
        var found = wanted ? find(wanted) : null;
        if (found) {
          drawer.hidden = false;
          part('toggle').setAttribute('aria-expanded', 'true');
          applyBookmark(found);
        } else if (wanted) {
          say('That bookmark is gone or is not shared with you.');
        }
      }).catch(fail);
    }

    return {
      mount: mount, bookmarkPath: bookmarkPath, bookmarkIdOf: bookmarkIdOf,
      endpoint: endpoint, send: send,
    };
  })();

  var ReportFilter = (function () {
    var UNASSIGNED = 'unassigned';
    var MODE_ALL = 'and';
    var MODE_ANY = 'or';

    function copyValues(values) {
      var out = {};
      Object.keys(values).forEach(function (key) {
        out[key] = Array.isArray(values[key]) ? values[key].slice() : values[key];
      });
      return out;
    }

    function create(controls, onReload) {
      var applied = {};
      var byId = {};
      controls.forEach(function (c) {
        byId[c.id] = c;
        applied[c.id] = c.multi ? (c.value || []).slice() : c.value;
      });
      var draft = copyValues(applied);
      var state = {
        controls: controls,
        draft: function () { return copyValues(draft); },
        applied: function () { return copyValues(applied); },
        set: function (id, value) { draft[id] = Array.isArray(value) ? value.slice() : value; },
        selectAll: function (id) {
          draft[id] = byId[id].options.map(function (o) { return o.value; });
        },
        clearAll: function (id) { draft[id] = []; },
        reload: function () {
          applied = copyValues(draft);
          onReload(copyValues(applied));
        },
        apply: function (values) {
          controls.forEach(function (c) {
            var next = values[c.id];
            var known = (c.options || []).map(function (o) { return o.value; });
            if (c.multi) {
              if (Array.isArray(next)) draft[c.id] = next.filter(function (v) { return known.indexOf(v) !== -1; });
            } else if (typeof next === 'string' && (!c.options || known.indexOf(next) !== -1)) {
              draft[c.id] = next;
            }
          });
          if (state.redraw) state.redraw();
          state.reload();
        },
      };
      return state;
    }

    function userControl(data) {
      return {
        id: 'user', label: 'User', value: '',
        options: [{ value: '', text: 'Show all' }, { value: UNASSIGNED, text: 'Unassigned' }].concat(
          (data.users || []).map(function (u) { return { value: u.id, text: u.username }; })),
      };
    }

    function colorControl(data) {
      return {
        id: 'color', label: 'Color', value: '',
        options: [{ value: '', text: 'Show all' }].concat(
          (data.colors || []).map(function (c) { return { value: c.id, text: c.label || c.value }; })),
      };
    }

    function labelControls(data) {
      var seen = Object.create(null);
      (data.tasks || []).forEach(function (t) {
        (t.labels || []).forEach(function (l) { seen[l] = true; });
      });
      return [{
        id: 'labels', label: 'Label', multi: true, value: [],
        options: Object.keys(seen).sort().map(function (l) { return { value: l, text: l }; }),
      }, {
        id: 'labelMode', label: 'Labels match', value: MODE_ALL,
        options: [{ value: MODE_ALL, text: 'All chosen labels' }, { value: MODE_ANY, text: 'Any chosen label' }],
      }];
    }

    function taskControls(data) {
      return [userControl(data), colorControl(data)].concat(labelControls(data));
    }

    function matches(task, values) {
      var members = task.member_ids || [];
      if (values.user === UNASSIGNED) {
        if (members.length) return false;
      } else if (values.user && members.indexOf(values.user) === -1) {
        return false;
      }
      if (values.color && task.color_id !== values.color) return false;
      var chosen = values.labels || [];
      if (!chosen.length) return true;
      var have = task.labels || [];
      function held(label) { return have.indexOf(label) !== -1; }
      return values.labelMode === MODE_ANY ? chosen.some(held) : chosen.every(held);
    }

    function filterTasks(tasks, values) {
      return tasks.filter(function (t) { return matches(t, values); });
    }

    function whenAttrs(c, values) {
      if (!c.when) return '';
      return ' data-when="' + escapeHtml(c.when.id) + '" data-when-value="' + escapeHtml(c.when.value) + '"' +
        (values[c.when.id] === c.when.value ? '' : ' hidden');
    }

    function syncVisibility(host, values) {
      Array.prototype.forEach.call(host.querySelectorAll('[data-when]'), function (el) {
        el.hidden = values[el.getAttribute('data-when')] !== el.getAttribute('data-when-value');
      });
    }

    function controlHtml(c, values) {
      var id = 'rf-' + c.id;
      var html;
      if (c.multi) {
        html = '<fieldset class="rf-multi" data-multi="' + escapeHtml(c.id) + '"><legend>' +
          escapeHtml(c.label) + '</legend>';
        if (!c.options.length) return html + '<p class="rf-empty">None</p></fieldset>';
        html += '<div class="rf-multi-actions">' +
          '<button type="button" class="rf-link" data-select-all="' + escapeHtml(c.id) + '">Select all</button>' +
          '<button type="button" class="rf-link" data-clear-all="' + escapeHtml(c.id) + '">Clear all</button>' +
          '</div><div class="rf-multi-list">';
        c.options.forEach(function (o) {
          html += '<label class="check-label"><input type="checkbox" data-control="' + escapeHtml(c.id) +
            '" value="' + escapeHtml(o.value) + '"' +
            (values[c.id].indexOf(o.value) !== -1 ? ' checked' : '') + '> ' + escapeHtml(o.text) + '</label>';
        });
        return html + '</div></fieldset>';
      }
      if (c.type === 'date' || c.type === 'number') {
        return '<label' + whenAttrs(c, values) + '>' + escapeHtml(c.label) + '<input type="' + c.type +
          '" id="' + id + '" data-control="' + escapeHtml(c.id) + '"' +
          (c.type === 'number' ? ' min="1" max="' + c.max + '"' : '') +
          ' value="' + escapeHtml(values[c.id] || '') + '"></label>';
      }
      html = '<label' + whenAttrs(c, values) + '>' + escapeHtml(c.label) + '<select id="' + id +
        '" data-control="' + escapeHtml(c.id) + '">';
      c.options.forEach(function (o) {
        html += '<option value="' + escapeHtml(o.value) + '"' +
          (values[c.id] === o.value ? ' selected' : '') + '>' + escapeHtml(o.text) + '</option>';
      });
      return html + '</select></label>';
    }

    function checkboxes(host, id) {
      return Array.prototype.filter.call(host.querySelectorAll('input[type="checkbox"]'), function (box) {
        return box.getAttribute('data-control') === id;
      });
    }

    function syncChecks(host, id, values) {
      checkboxes(host, id).forEach(function (box) { box.checked = values.indexOf(box.value) !== -1; });
    }

    function mount(host, state) {
      function draw() {
        var values = state.draft();
        var html = '<h2>Filter</h2>';
        state.controls.forEach(function (c) { html += controlHtml(c, values); });
        host.innerHTML = html + '<button type="button" class="kf-btn kf-btn-primary" data-reload>Reload</button>';
      }
      state.redraw = draw;
      draw();
      host.addEventListener('change', function (e) {
        var id = e.target.getAttribute('data-control');
        if (!id) return;
        if (e.target.type === 'checkbox') {
          state.set(id, checkboxes(host, id).filter(function (box) { return box.checked; })
            .map(function (box) { return box.value; }));
        } else {
          state.set(id, e.target.value);
        }
        syncVisibility(host, state.draft());
      });
      host.addEventListener('click', function (e) {
        var t = e.target;
        if (t.hasAttribute('data-reload')) {
          state.reload();
        } else if (t.hasAttribute('data-select-all')) {
          var all = t.getAttribute('data-select-all');
          state.selectAll(all);
          syncChecks(host, all, state.draft()[all]);
        } else if (t.hasAttribute('data-clear-all')) {
          var none = t.getAttribute('data-clear-all');
          state.clearAll(none);
          syncChecks(host, none, []);
        }
      });
    }

    function open(host, controls, render, fail) {
      var state = create(controls, function (values) {
        try {
          render(values);
        } catch (err) {
          fail(err);
        }
      });
      if (host) mount(host, state);
      state.reload();
      return state;
    }

    return {
      create: create, mount: mount, open: open, matches: matches, filterTasks: filterTasks,
      taskControls: taskControls, colorControl: colorControl, labelControls: labelControls,
      MODE_ALL: MODE_ALL, MODE_ANY: MODE_ANY,
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

    // KF-350: bands are columns keyed by id. An event that carries a
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
      // KF-350: the timeline ends in the task's current column, whatever
      // name its last event recorded.
      if (home && tl.length > 0) tl[tl.length - 1].col = home;
      return tl;
    }

    return { lookup: columnLookup, eventKey: eventKey, timelineFor: timelineFor };
  })();

  var BurndownPage = (function () {
    var initialized = false;
    var DAYS = 30;
    var BASIS_DAYS = [7, 14, 21, 30, 60, 90, 120];
    var DAY_MS = ReportRange.DAY_MS;
    var MAX_PROJECTION_DAYS = 365;
    var GEO = { W: 720, H: 300, padL: 46, padR: 32, padT: 16, padB: 34 };

    var roundTenth = ReportRange.roundTenth;

    var VALUE_TYPES = {
      count: {
        text: ReportValue.count.text,
        axis: ReportValue.count.axis,
        remaining: 'Tasks remaining',
        projected: Math.ceil,
        valueOf: ReportValue.count.valueOf,
        title: function (n) { return n + ' tasks remaining'; },
        status: function (n) { return n === 1 ? '1 task remaining.' : n + ' tasks remaining.'; },
      },
      estimate: {
        text: ReportValue.estimate.text,
        axis: ReportValue.estimate.axis,
        remaining: 'Hours remaining',
        projected: roundTenth,
        valueOf: ReportValue.estimate.valueOf,
        title: function (n) { return roundTenth(n) + ' hours remaining'; },
        status: function (n) { return roundTenth(n) + ' hours remaining.'; },
      },
      points: {
        text: ReportValue.points.text,
        axis: ReportValue.points.axis,
        remaining: 'Points remaining',
        projected: roundTenth,
        valueOf: ReportValue.points.valueOf,
        title: function (n) { return roundTenth(n) + ' points remaining'; },
        status: function (n) { return roundTenth(n) + ' points remaining.'; },
      },
    };

    function typeOf(series) { return VALUE_TYPES[series.type] || VALUE_TYPES.count; }

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('burndown-status');
      if (el) el.textContent = msg;
    }

    function firstArrival(timeline, column) {
      for (var i = 0; i < timeline.length; i++) {
        if (timeline[i].col === column) return timeline[i].at;
      }
      return null;
    }

    function remainingByDay(tasks, opts) {
      var ends = ReportRange.dayEnds(opts.start, new Date());
      var type = VALUE_TYPES[opts.valueType] || VALUE_TYPES.count;
      var from = null;
      if (opts.from) {
        from = Object.create(null);
        opts.from.forEach(function (id) { from[id] = true; });
      }
      var counts = ends.map(function () { return 0; });
      var burned = [];
      tasks.forEach(function (t) {
        var created = Date.parse(t.created_at);
        if (isNaN(created)) return;
        var value = type.valueOf(t);
        var done = t.completed_at ? Date.parse(t.completed_at) : NaN;
        var timeline = ColumnHistory.timelineFor(t, opts.cols);
        var arrived = opts.targetDone ? (isNaN(done) ? null : done) : firstArrival(timeline, opts.target);
        var inScope = !from || timeline.some(function (entry) { return from[entry.col]; });
        if (arrived !== null && inScope) burned.push({ at: arrived, value: value });
        var p = -1;
        ends.forEach(function (e, di) {
          var ms = e.getTime();
          if (created > ms) return;
          while (p + 1 < timeline.length && timeline[p + 1].at <= ms) p++;
          var col = p >= 0 ? timeline[p].col : null;
          if (opts.targetDone ? (!isNaN(done) && done <= ms) : col === opts.target) return;
          if (from && !from[col]) return;
          counts[di] += value;
        });
      });
      var basis = opts.basis || DAYS;
      var windowStart = new Date(ends[ends.length - 1]);
      windowStart.setDate(windowStart.getDate() - basis);
      windowStart = windowStart.getTime();
      var total = 0;
      burned.forEach(function (b) { if (b.at > windowStart) total += b.value; });
      var series = projectSeries({ ends: ends, counts: counts }, total / basis);
      series.type = opts.valueType;
      return series;
    }

    function projectSeries(series, rate) {
      var counts = series.counts;
      var left = counts.length ? counts[counts.length - 1] : 0;
      var ends = series.ends.slice();
      var projection = [];
      if (left > 0 && rate > 0) {
        var days = Math.min(MAX_PROJECTION_DAYS, Math.ceil(left / rate));
        var last = ends[ends.length - 1];
        for (var k = 1; k <= days; k++) {
          var d = new Date(last);
          d.setDate(d.getDate() + k);
          ends.push(d);
          projection.push(Math.max(0, left - rate * k));
        }
      }
      return { ends: ends, counts: counts, projection: projection, type: series.type };
    }

    function chartSvg(series) {
      var W = GEO.W, H = GEO.H, padL = GEO.padL, padR = GEO.padR, padT = GEO.padT, padB = GEO.padB;
      var innerW = W - padL - padR, innerH = H - padT - padB;
      var type = typeOf(series);
      var max = 1;
      series.counts.forEach(function (n) { if (n > max) max = n; });
      max = Math.ceil(max / 5) * 5;

      var n = series.ends.length;
      var actual = series.counts.length;
      var projection = series.projection || [];
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
        ' transform="rotate(-90 14 ' + midY + ')">' + escapeHtml(type.axis) + '</text>';
      var i, j, k;
      var step = Math.max(5, Math.ceil(n / 7));
      var labelled = [];
      for (i = 0; i < n; i += step) labelled.push(i);
      if (projection.length && n - 1 - labelled[labelled.length - 1] >= 3) labelled.push(n - 1);
      labelled.forEach(function (at) {
        svg += '<text x="' + x(at).toFixed(1) + '" y="' + (H - 10) +
          '" text-anchor="middle" class="chart-tick">' +
          escapeHtml(ReportChart.shortDate(series.ends[at])) + '</text>';
      });
      var pts = [];
      for (j = 0; j < actual; j++) {
        pts.push(x(j).toFixed(1) + ',' + y(series.counts[j]).toFixed(1));
      }
      if (actual) {
        svg += '<polygon points="' + x(0).toFixed(1) + ',' + y(0).toFixed(1) + ' ' + pts.join(' ') +
          ' ' + x(actual - 1).toFixed(1) + ',' + y(0).toFixed(1) + '" class="chart-area"/>';
      }
      svg += '<polyline points="' + pts.join(' ') + '" class="chart-line" fill="none"/>';
      if (projection.length) {
        var ppts = [pts[actual - 1]];
        projection.forEach(function (v, p) {
          ppts.push(x(actual + p).toFixed(1) + ',' + y(v).toFixed(1));
        });
        svg += '<polyline points="' + ppts.join(' ') + '" class="chart-projection" fill="none"/>';
      }
      for (k = 0; k < actual; k++) {
        svg += '<circle cx="' + x(k).toFixed(1) + '" cy="' + y(series.counts[k]).toFixed(1) +
          '" r="4" class="chart-dot"><title>' + escapeHtml(type.title(series.counts[k])) +
          ', ' + escapeHtml(ReportChart.longDate(ReportChart.dayStart(series.ends[k]))) + '</title></circle>';
      }
      svg += ReportChart.crosshairLine(GEO) + '</svg>';
      return svg;
    }

    function tipFor(series, i) {
      var type = typeOf(series);
      var actual = i < series.counts.length;
      return {
        title: ReportChart.longDate(ReportChart.dayStart(series.ends[i])),
        rows: [{
          label: actual ? type.remaining : 'Projected remaining',
          value: actual ? roundTenth(series.counts[i]) : type.projected(series.projection[i - series.counts.length]),
        }],
      };
    }

    var lastSeries = null;

    function rows() {
      if (!lastSeries) return [];
      var series = lastSeries;
      var actual = series.counts.length;
      var out = [['Date', typeOf(series).axis, 'Projection']];
      series.ends.forEach(function (end, i) {
        out.push([isoDate(end), i < actual ? roundTenth(series.counts[i]) : '',
          i >= actual && series.projection ? roundTenth(series.projection[i - actual]) : '']);
      });
      return out;
    }

    function renderChart(series) {
      lastSeries = series;
      var host = qs('burndown-chart');
      if (host) {
        host.innerHTML = chartSvg(series);
        ReportChart.attachCrosshair(host, GEO, series.ends.length, function (i) { return tipFor(series, i); });
      }
    }

    function clearChart() {
      lastSeries = null;
      var host = qs('burndown-chart');
      if (host) host.innerHTML = '';
    }

    function defaultTarget(columns) {
      var done = columns.filter(function (c) { return c.is_done; })[0] || columns[columns.length - 1];
      return done ? done.id : '';
    }

    function controls(data) {
      var columns = data.columns || [];
      var options = columns.map(function (c) { return { value: c.id, text: c.name }; });
      var start = new Date();
      start.setDate(start.getDate() - DAYS);
      return [{
        id: 'valueType', label: 'Value type', value: 'count',
        options: Object.keys(VALUE_TYPES).map(function (key) {
          return { value: key, text: VALUE_TYPES[key].text };
        }),
      }, {
        id: 'target', label: 'Target column', value: defaultTarget(columns), options: options,
      }, {
        id: 'from', label: 'From columns', multi: true,
        value: columns.map(function (c) { return c.id; }), options: options,
      }, {
        id: 'start', label: 'Start date', type: 'date', value: isoDate(start),
      }, {
        id: 'basis', label: 'Projection basis', value: String(DAYS),
        options: BASIS_DAYS.map(function (n) { return { value: String(n), text: 'Last ' + n + ' days' }; }),
      }].concat(ReportFilter.taskControls(data));
    }

    function render(data, values) {
      var columns = data.columns || [];
      var start = ReportBreakdown.parseDay(values.start);
      var today = new Date();
      var earliest = new Date(today.getFullYear(), today.getMonth(), today.getDate() - ReportRange.MAX_DAYS).getTime();
      var problem = null;
      if (start === null || start > today.getTime()) problem = 'Pick a start date on or before today.';
      else if (start < earliest) problem = 'Pick a start date within the last ' + ReportRange.MAX_YEARS + ' years.';
      else if (!values.from.length) problem = 'Choose at least one From column.';
      if (problem) {
        clearChart();
        setStatus(problem);
        return;
      }
      var target = columns.filter(function (c) { return c.id === values.target; })[0];
      var tasks = ReportFilter.filterTasks(data.tasks || [], values);
      var series = remainingByDay(tasks, {
        start: new Date(start),
        valueType: values.valueType,
        basis: parseInt(values.basis, 10),
        target: values.target,
        targetDone: !!(target && target.is_done),
        from: values.from.length === columns.length ? null : values.from,
        cols: ColumnHistory.lookup(data),
      });
      renderChart(series);
      var current = series.counts.length ? series.counts[series.counts.length - 1] : 0;
      if ((data.tasks || []).length === 0) setStatus('No tasks on this board.');
      else if (tasks.length === 0) setStatus('No tasks match the filter.');
      else setStatus(typeOf(series).status(current));
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' },
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        var state = ReportFilter.open(qs('report-filter'), controls(data), function (values) {
          render(data, values);
        }, function (err) {
          setStatus('Could not draw the chart: ' + err.message);
        });
        ReportTools.mount({
          report: 'burndown', title: 'Burndown', boardId: boardId, chartId: 'burndown-chart',
          getFilters: state.applied, applyFilters: state.apply, rows: rows,
        });
      }).catch(function (err) {
        setStatus('Could not load task data: ' + err.message);
      });
    }

    return {
      init: init, chartSvg: chartSvg, tipFor: tipFor, projectSeries: projectSeries,
      remainingByDay: remainingByDay, controls: controls, render: render,
    };
  })();

  // KF-340: Cycle & lead time report page. Average cycle/lead time per week
  // (last 12 weeks) with a 3-week moving-average trend line,
  // rendered from the shared export-data dataset.
  // Definitions (KanbanFlow): lead time = creation -> completion; cycle time
  // = first column move -> completion (falls back to creation when the task
  // was never moved).
  var CycleLeadPage = (function () {
    var initialized = false;
    var WEEKS = 12;
    var WEEK_MS = 7 * 86400000;
    var metric = 'cycle';
    var hidden = {};

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('cycle-lead-status');
      if (el) el.textContent = msg;
    }

    // First day of the account's week (local) containing d.
    function weekStart(d) {
      var x = new Date(d.getFullYear(), d.getMonth(), d.getDate());
      x.setDate(x.getDate() - daysIntoWeek(x));
      return x;
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
        var started = null;
        (t.history || []).forEach(function (e) {
          if (e && e.kind === 'moved') {
            var d = new Date(e.created_at);
            if (!isNaN(d.getTime()) && (!started || d.getTime() < started.getTime())) started = d;
          }
        });
        if (!started || started.getTime() < created.getTime()) started = created;
        out.push({
          id: t.id,
          name: t.name,
          doneAt: done.getTime(),
          doneWeek: weekStart(done).getTime(),
          fromAt: { cycle: started.getTime(), lead: created.getTime() },
          cycle: daysBetween(started, done),
          lead: daysBetween(created, done),
        });
      });
      return out;
    }

    // 12 week buckets from the account's first day of the week (oldest first); each holds sum/count
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
      var inWindow = times.filter(function (t) { return starts.indexOf(t.doneWeek) !== -1; });
      return {
        starts: starts.map(function (ms) { return new Date(ms); }),
        tasks: inWindow,
        avg: buckets.map(function (b) {
          return {
            cycle: b.cycleN ? b.cycleSum / b.cycleN : 0,
            lead: b.leadN ? b.leadSum / b.leadN : 0,
            n: b.cycleN,
          };
        }),
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

    function taskDots(data, tasks, metric, x, y) {
      var n = data.starts.length;
      var from = data.starts[0].getTime();
      var span = n * WEEK_MS;
      var out = '';
      tasks.forEach(function (tk) {
        var f = Math.min(n - 1, Math.max(0, (tk.doneAt - from) / span * (n - 1)));
        out += '<circle cx="' + x(f).toFixed(1) + '" cy="' + y(tk[metric]).toFixed(1) +
          '" r="3.5" class="chart-task-dot" tabindex="0" role="button"' +
          ' data-task-id="' + escapeHtml(tk.id) + '"' +
          ' data-name="' + escapeHtml(tk.name) + '"' +
          ' data-from="' + escapeHtml(ReportChart.longDate(new Date(tk.fromAt[metric]))) + '"' +
          ' data-to="' + escapeHtml(ReportChart.longDate(new Date(tk.doneAt))) + '"' +
          ' data-days="' + escapeHtml(fmtDays(tk[metric])) + '"' +
          ' aria-label="' + escapeHtml(tk.name) + ', ' + escapeHtml(fmtDays(tk[metric])) + '"/>';
      });
      return out;
    }

    function chartSvg(data, view) {
      var metric = view.metric;
      var hidden = view.hidden;
      var tasks = data.tasks || [];
      var W = 720, H = 300, padL = 52, padR = 32, padT = 16, padB = 34;
      var innerW = W - padL - padR, innerH = H - padT - padB;
      var vals = data.avg.map(function (a) { return a[metric]; });
      var trend = movingAverage(vals);
      var max = 1;
      vals.concat(trend).forEach(function (v) { if (v > max) max = v; });
      tasks.forEach(function (tk) { if (tk[metric] > max) max = tk[metric]; });
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
          escapeHtml(ReportChart.shortDate(data.starts[i])) + '</text>';
      }
      if (!hidden.trend) {
        var tp = [];
        for (j = 0; j < n; j++) {
          tp.push(x(j).toFixed(1) + ',' + y(trend[j]).toFixed(1));
        }
        svg += '<polyline points="' + tp.join(' ') + '" class="chart-trend" fill="none"/>';
      }
      if (!hidden.metric) {
        var pts = [];
        for (j = 0; j < n; j++) {
          pts.push(x(j).toFixed(1) + ',' + y(vals[j]).toFixed(1));
        }
        svg += '<polyline points="' + pts.join(' ') + '" class="chart-line" fill="none"/>';
        for (k = 0; k < n; k++) {
          svg += '<circle cx="' + x(k).toFixed(1) + '" cy="' + y(vals[k]).toFixed(1) +
            '" r="4" class="chart-dot"><title>' + escapeHtml(fmtDays(vals[k])) +
            ' avg, week of ' + escapeHtml(ReportChart.longDate(data.starts[k])) +
            ' (' + data.avg[k].n + (data.avg[k].n === 1 ? ' task' : ' tasks') + ')</title></circle>';
        }
        svg += taskDots(data, tasks, metric, x, y);
      }
      svg += '</svg>';
      return svg;
    }

    function renderChart(data) {
      var host = qs('cycle-lead-chart');
      if (host) host.innerHTML = chartSvg(data, { metric: metric, hidden: hidden });
    }

    var lastData = null;

    function toggleSeries(key) {
      hidden[key] = !hidden[key];
      var btn = document.querySelector('[data-series="' + key + '"]');
      if (btn) btn.setAttribute('aria-pressed', hidden[key] ? 'false' : 'true');
      if (lastData) renderChart(lastData);
    }

    function setMetric(m) {
      metric = m;
      var cb = qs('cycle-lead-cycle'), lb = qs('cycle-lead-lead');
      if (cb) { cb.classList.toggle('kf-btn-active', m === 'cycle'); cb.setAttribute('aria-pressed', m === 'cycle' ? 'true' : 'false'); }
      if (lb) { lb.classList.toggle('kf-btn-active', m === 'lead'); lb.setAttribute('aria-pressed', m === 'lead' ? 'true' : 'false'); }
      if (lastData) renderChart(lastData);
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      var cb = qs('cycle-lead-cycle'), lb = qs('cycle-lead-lead');
      if (cb) cb.addEventListener('click', function () { setMetric('cycle'); });
      if (lb) lb.addEventListener('click', function () { setMetric('lead'); });
      Array.prototype.forEach.call(document.querySelectorAll('[data-series]'), function (btn) {
        btn.addEventListener('click', function () { toggleSeries(btn.getAttribute('data-series')); });
      });
      var host = qs('cycle-lead-chart');
      if (host) ReportChart.attachDots(host, '.chart-task-dot', openModal);
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' },
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        var times = computeTimes(data.tasks || []);
        var buckets = bucketize(times);
        lastData = buckets;
        renderChart(buckets);
        ReportTools.mount({
          report: 'cycle-lead', title: 'Cycle and lead time', boardId: boardId, chartId: 'cycle-lead-chart',
          getFilters: function () { return { metric: metric }; },
          applyFilters: function (values) {
            if (values.metric === 'cycle' || values.metric === 'lead') setMetric(values.metric);
          },
          rows: function () {
            return [['Week starting', 'Average cycle time (days)', 'Average lead time (days)', 'Tasks completed']]
              .concat(buckets.starts.map(function (start, i) {
                return [isoDate(start), buckets.avg[i].cycle, buckets.avg[i].lead, buckets.avg[i].n];
              }));
          },
        });
        var total = times.length;
        setStatus(total === 0
          ? 'No completed tasks in the last 12 weeks.'
          : (total === 1 ? '1 task completed in the last 12 weeks.'
                         : total + ' tasks completed in the last 12 weeks.'));
      }).catch(function (err) {
        setStatus('Could not load task data: ' + err.message);
      });
    }

    return { init: init, chartSvg: chartSvg, computeTimes: computeTimes, bucketize: bucketize };
  })();

  // KF-339: Cumulative flow report page — tasks in each column over time
  // as a stacked area chart. Reconstructed from each task's history trail
  // ("created"/"moved" events), falling back to the task's current column
  // when it has no recorded moves.
  var CumulativeFlowPage = (function () {
    var initialized = false;
    var DAYS = 30;
    var hiddenBands = {};
    var GEO = { W: 720, H: 320, padL: 46, padR: 32, padT: 16, padB: 34 };

    // One color per column band.
    var PALETTE = ['#4e79a7', '#f28e2b', '#e15759', '#76b7b2', '#59a14f',
      '#edc948', '#b07aa1', '#ff9da7', '#9c755f', '#bab0ac'];

    var roundTenth = ReportRange.roundTenth;
    var PERIOD = { THIS_MONTH: 'this-month', LAST_MONTH: 'last-month', ABSOLUTE: 'absolute', RELATIVE: 'relative' };

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('cumulative-flow-status');
      if (el) el.textContent = msg;
    }

    // Band order: the board's runtime columns first, then any deleted
    // columns history still names, in first-appearance order, so the chart
    // always sums to the true total.
    function columnOrder(data, cols) {
      var order = (data.columns || []).map(function (c) { return c.id; });
      var seen = {};
      order.forEach(function (k) { seen[k] = true; });
      (data.tasks || []).forEach(function (t) {
        (t.history || []).forEach(function (e) {
          var key = ColumnHistory.eventKey(e, cols);
          if (key && !seen[key]) { seen[key] = true; order.push(key); }
        });
      });
      return order;
    }

    function lastDays(n, now) {
      var start = new Date(now.getFullYear(), now.getMonth(), now.getDate() - (n - 1));
      return { start: start, end: now };
    }

    function periodRange(values, now) {
      var today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
      var y = today.getFullYear(), m = today.getMonth();
      var range;
      if (values.period === PERIOD.THIS_MONTH) {
        range = { start: new Date(y, m, 1), end: today };
      } else if (values.period === PERIOD.LAST_MONTH) {
        range = { start: new Date(y, m - 1, 1), end: new Date(y, m, 0) };
      } else if (values.period === PERIOD.ABSOLUTE) {
        var a = ReportBreakdown.parseDay(values.from), b = ReportBreakdown.parseDay(values.to);
        if (a === null || b === null || b < a) return null;
        range = { start: new Date(a), end: new Date(Math.min(b, today.getTime())) };
        if (range.end < range.start) return null;
      } else if (values.period === PERIOD.RELATIVE) {
        var n = parseInt(values.days, 10);
        if (!(n >= 1)) return null;
        range = lastDays(n, today);
      } else {
        range = lastDays(parseInt(values.period, 10) || DAYS, today);
      }
      if ((range.end - range.start) / ReportRange.DAY_MS > ReportRange.MAX_DAYS) return null;
      return range;
    }

    function seriesByDay(data, counted, range, type) {
      var valueOf = (ReportValue[type] || ReportValue.count).valueOf;
      var span = range || lastDays(DAYS, new Date());
      var ends = ReportRange.dayEnds(span.start, span.end);
      var cols = ColumnHistory.lookup(data);
      var order = columnOrder(data, cols);
      var idx = {};
      order.forEach(function (key, k) { idx[key] = k; });
      var counts = order.map(function () {
        return ends.map(function () { return 0; });
      });
      (counted || data.tasks || []).forEach(function (t) {
        var tl = ColumnHistory.timelineFor(t, cols);
        if (tl.length === 0) return;
        var value = valueOf(t);
        var p = 0;
        ends.forEach(function (e, di) {
          var ms = e.getTime();
          while (p + 1 < tl.length && tl[p + 1].at <= ms) p++;
          if (tl[p].at <= ms && idx[tl[p].col] !== undefined) counts[idx[tl[p].col]][di] += value;
        });
      });
      // KF-350: a band only history knows (a deleted column, or a name no
      // column claims) that holds no task in the window is dropped, so the
      // chart, legend and status show only real bands.
      var keep = order.map(function (key, k) {
        return cols.current[key] || counts[k].some(function (c) { return c > 0; });
      });
      return {
        ends: ends,
        order: order.filter(function (_, k) { return keep[k]; })
          .map(function (key) { return cols.label[key]; }),
        counts: counts.filter(function (_, k) { return keep[k]; }),
        type: type,
      };
    }

    function shownBands(s) {
      var shown = [];
      s.order.forEach(function (name, b) {
        if (!(s.hidden && s.hidden[name])) shown.push(b);
      });
      return shown;
    }

    function chartSvg(s) {
      var shown = shownBands(s);
      var axis = (ReportValue[s.type] || ReportValue.count).axis;
      var W = GEO.W, H = GEO.H, padL = GEO.padL, padR = GEO.padR, padT = GEO.padT, padB = GEO.padB;
      var innerW = W - padL - padR, innerH = H - padT - padB;
      var n = s.ends.length;
      var totals = s.ends.map(function (_, di) {
        var t = 0;
        shown.forEach(function (b) { t += s.counts[b][di]; });
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
        ' transform="rotate(-90 14 ' + midY + ')">' + escapeHtml(axis) + '</text>';
      var i;
      var step = Math.max(5, Math.ceil(n / 7));
      for (i = 0; i < n; i += step) {
        svg += '<text x="' + x(i).toFixed(1) + '" y="' + (H - 10) +
          '" text-anchor="middle" class="chart-tick">' +
          escapeHtml(ReportChart.shortDate(s.ends[i])) + '</text>';
      }
      // Bands: the board's last column is the bottom band (the canonical
      // CFD reading: completed work grows along the bottom).
      var m = s.order.length;
      var lower = [];
      for (var di = 0; di < n; di++) lower.push(0);
      for (var b = m - 1; b >= 0; b--) {
        if (shown.indexOf(b) === -1) continue;
        var upper = lower.map(function (lo, k) { return lo + s.counts[b][k]; });
        var d = 'M' + x(0).toFixed(1) + ',' + y(upper[0]).toFixed(1);
        for (var j = 1; j < n; j++) d += ' L' + x(j).toFixed(1) + ',' + y(upper[j]).toFixed(1);
        for (var k = n - 1; k >= 0; k--) d += ' L' + x(k).toFixed(1) + ',' + y(lower[k]).toFixed(1);
        d += ' Z';
        var color = PALETTE[b % PALETTE.length];
        var peak = 0;
        s.counts[b].forEach(function (c) { if (c > peak) peak = c; });
        svg += '<path d="' + d + '" fill="' + color + '" fill-opacity="0.75" stroke="' + color + '">' +
          '<title>' + escapeHtml(s.order[b]) + ': up to ' + roundTenth(peak) + ' ' + (ReportValue[s.type] || ReportValue.count).unit + '</title></path>';
        lower = upper;
      }
      svg += ReportChart.crosshairLine(GEO) + '</svg>';
      return svg;
    }

    function tipFor(s, i) {
      return {
        title: ReportChart.longDate(ReportChart.dayStart(s.ends[i])),
        rows: shownBands(s).map(function (b) {
          return { color: PALETTE[b % PALETTE.length], label: s.order[b], value: roundTenth(s.counts[b][i]) };
        }),
      };
    }

    var current = null;

    function rows() {
      if (!current) return [];
      return [['Date'].concat(current.order)].concat(current.ends.map(function (end, i) {
        return [isoDate(end)].concat(current.counts.map(function (band) { return roundTenth(band[i]); }));
      }));
    }

    function renderChart(s) {
      current = s;
      s.hidden = hiddenBands;
      var host = qs('cumulative-flow-chart');
      if (host) {
        host.innerHTML = chartSvg(s);
        ReportChart.attachCrosshair(host, GEO, s.ends.length, function (i) { return tipFor(s, i); });
      }

      // Legend in board column order.
      var legend = qs('cumulative-flow-legend');
      if (legend) {
        var html = '';
        s.order.forEach(function (name, bi) {
          html += '<button type="button" class="cfd-legend-item" data-band="' + bi + '" aria-pressed="' +
            (hiddenBands[name] ? 'false' : 'true') + '"><span class="cfd-swatch" style="background:' +
            PALETTE[bi % PALETTE.length] + '"></span>' + escapeHtml(name) + '</button>';
        });
        legend.innerHTML = html;
      }
    }

    function toggleBand(index) {
      if (!current || !current.order[index]) return;
      var name = current.order[index];
      hiddenBands[name] = !hiddenBands[name];
      renderChart(current);
    }

    function unitText(type, n) {
      var unit = (ReportValue[type] || ReportValue.count).unit;
      return roundTenth(n) + ' ' + (n === 1 ? unit.replace(/s$/, '') : unit);
    }

    function controls(data) {
      var now = new Date();
      var from = new Date(now.getFullYear(), now.getMonth(), now.getDate() - DAYS);
      return [{
        id: 'valueType', label: 'Value type', value: 'count',
        options: Object.keys(ReportValue).map(function (key) {
          return { value: key, text: ReportValue[key].text };
        }),
      }, {
        id: 'period', label: 'Period', value: String(DAYS),
        options: [
          { value: '7', text: 'Last 7 days' },
          { value: '30', text: 'Last 30 days' },
          { value: PERIOD.THIS_MONTH, text: 'This month' },
          { value: PERIOD.LAST_MONTH, text: 'Last month' },
          { value: PERIOD.ABSOLUTE, text: 'Custom absolute' },
          { value: PERIOD.RELATIVE, text: 'Custom relative' },
        ],
      }, {
        id: 'from', label: 'From', type: 'date', value: isoDate(from),
        when: { id: 'period', value: PERIOD.ABSOLUTE },
      }, {
        id: 'to', label: 'To', type: 'date', value: isoDate(now),
        when: { id: 'period', value: PERIOD.ABSOLUTE },
      }, {
        id: 'days', label: 'Days back', type: 'number', max: ReportRange.MAX_DAYS, value: String(DAYS),
        when: { id: 'period', value: PERIOD.RELATIVE },
      }].concat(ReportFilter.taskControls(data));
    }

    function render(data, values) {
      var range = periodRange(values, new Date());
      if (!range) {
        current = null;
        var blank = qs('cumulative-flow-chart');
        if (blank) blank.innerHTML = '';
        var emptyLegend = qs('cumulative-flow-legend');
        if (emptyLegend) emptyLegend.innerHTML = '';
        setStatus('Pick a valid period of at most ' + ReportRange.MAX_YEARS + ' years that starts on or before today.');
        return;
      }
      var tasks = ReportFilter.filterTasks(data.tasks || [], values);
      var s = seriesByDay(data, tasks, range, values.valueType);
      renderChart(s);
      var total = 0;
      if (s.ends.length) {
        var last = s.ends.length - 1;
        s.counts.forEach(function (c) { total += c[last]; });
      }
      if ((data.tasks || []).length === 0) setStatus('No tasks on this board.');
      else if (tasks.length === 0) setStatus('No tasks match the filter.');
      else setStatus(unitText(s.type, total) + ' across ' + s.order.length + ' columns on the last day of the period.');
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' },
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        var legend = qs('cumulative-flow-legend');
        if (legend) {
          legend.addEventListener('click', function (e) {
            var button = e.target.closest ? e.target.closest('[data-band]') : null;
            if (button) toggleBand(parseInt(button.getAttribute('data-band'), 10));
          });
        }
        var state = ReportFilter.open(qs('report-filter'), controls(data), function (values) {
          render(data, values);
        }, function (err) {
          setStatus('Could not draw the chart: ' + err.message);
        });
        ReportTools.mount({
          report: 'cumulative-flow', title: 'Cumulative flow', boardId: boardId,
          chartId: 'cumulative-flow-chart', getFilters: state.applied, applyFilters: state.apply, rows: rows,
        });
      }).catch(function (err) {
        setStatus('Could not load task data: ' + err.message);
      });
    }

    // seriesByDay is pure; exposed for the KF-350 check.
    return {
      init: init, seriesByDay: seriesByDay, chartSvg: chartSvg, tipFor: tipFor,
      periodRange: periodRange, controls: controls, render: render,
    };
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
        ', ' + clockText(d);
    }

    // Classify each task that has a parseable due date.
    function classify(tasks) {
      var now = Date.now();
      var rows = [];
      tasks.forEach(function (t) {
        if (!t.due_at) return;
        var due = parseDueAt(t.due_at);
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
          cls: cls,
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
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' },
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

  const TimelinePage = (() => {
    const DAY_MS = 86400000;
    const MIN_ZOOM = 30;
    const MAX_ZOOM = 80;
    const ROW_HEIGHT = 28;
    const LEAD_DAYS = 7;
    const TRAIL_DAYS = 14;
    const MIN_DAYS = 45;
    const MIN_ROWS = 3;
    const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

    let initialized = false;
    let firstLoad = true;
    let dayWidth = MIN_ZOOM;
    let viewFirst = 0;
    let query = '';
    const folded = new Set();
    let boardId = '';
    let bars = [];
    let columns = [];
    let tasks = [];

    const qs = (id) => document.getElementById(id);
    const setStatus = (message) => { qs('tlp-status').textContent = message; };

    const make = (tag, className, text) => {
      const node = document.createElement(tag);
      if (className) node.className = className;
      if (text !== undefined) node.textContent = text;
      return node;
    };

    const dayNumber = (year, month, day) => Date.UTC(year, month, day) / DAY_MS;
    const localDay = (date) => dayNumber(date.getFullYear(), date.getMonth(), date.getDate());
    const dayDate = (number) => new Date(number * DAY_MS);
    const isoDay = (iso) => {
      const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(iso || '');
      return match ? dayNumber(Number(match[1]), Number(match[2]) - 1, Number(match[3])) : null;
    };
    const isoOf = (number) => dayDate(number).toISOString().slice(0, 10);
    const canWrite = () => qs('tlp-canvas').dataset.writes === 'true';
    const clampDelta = (bar, mode, delta) => {
      if (mode === 'start') return Math.min(delta, bar.end - bar.start);
      if (mode === 'end') return Math.max(delta, bar.start - bar.end);
      return delta;
    };
    const shifted = (bar, mode, delta) => {
      const step = clampDelta(bar, mode, delta);
      return {
        start: mode === 'end' ? bar.start : bar.start + step,
        end: mode === 'start' ? bar.end : bar.end + step,
      };
    };
    const isWeekend = (number) => [0, 6].includes(dayDate(number).getUTCDay());

    const palette = () => {
      const byId = new Map();
      let fallback = null;
      document.querySelectorAll('#board-colors span[data-id]').forEach((node) => {
        const entry = { background: node.dataset.bg, border: node.dataset.border };
        byId.set(node.dataset.id, entry);
        if (node.dataset.default === 'true') fallback = entry;
      });
      return { byId, fallback };
    };

    const cardsOf = (tasks) => (tasks || []).filter((task) => task.kind !== 'epic');

    const collectBars = (data) => {
      const columns = new Map((data.columns || []).map((column) => [column.id, column.name]));
      const colors = palette();
      return (data.tasks || []).flatMap((task) => {
        const start = isoDay(task.timeline_start);
        const end = isoDay(task.timeline_end);
        if (start === null || end === null || end < start) return [];
        const due = task.due_at ? new Date(task.due_at) : null;
        return [{
          id: task.id,
          name: task.name,
          column: columns.get(task.column_id) || '',
          start,
          end,
          due: due && !Number.isNaN(due.getTime()) ? localDay(due) : null,
          color: colors.byId.get(task.color_id) || colors.fallback,
        }];
      }).sort((a, b) => a.start - b.start || a.end - b.end || a.name.localeCompare(b.name));
    };

    const rangeOf = (list, today) => {
      let first = today;
      let last = today;
      list.forEach((bar) => {
        first = Math.min(first, bar.start, bar.due ?? bar.start);
        last = Math.max(last, bar.end, bar.due ?? bar.end);
      });
      first -= LEAD_DAYS;
      last += TRAIL_DAYS;
      return { first, last: Math.max(last, first + MIN_DAYS - 1) };
    };

    const monthRow = (first, last) => {
      const row = make('div', 'tlp-months');
      let cursor = first;
      while (cursor <= last) {
        const date = dayDate(cursor);
        const next = dayNumber(date.getUTCFullYear(), date.getUTCMonth() + 1, 1);
        const end = Math.min(next - 1, last);
        const cell = make('div', 'tlp-month', `${MONTHS[date.getUTCMonth()]} ${date.getUTCFullYear()}`);
        cell.style.width = `${(end - cursor + 1) * dayWidth}px`;
        row.append(cell);
        cursor = end + 1;
      }
      return row;
    };

    const dayRow = (first, last, today) => {
      const row = make('div', 'tlp-days');
      for (let day = first; day <= last; day++) {
        const cell = make('div', 'tlp-day', String(dayDate(day).getUTCDate()));
        cell.style.width = `${dayWidth}px`;
        if (isWeekend(day)) cell.classList.add('tlp-weekend');
        if (day === today) cell.classList.add('tlp-today-day');
        row.append(cell);
      }
      return row;
    };

    const barNode = (bar, index, first) => {
      const node = make('div', 'tlp-bar');
      node.setAttribute('role', 'button');
      node.tabIndex = 0;
      node.dataset.taskId = bar.id;
      node.style.left = `${(bar.start - first) * dayWidth}px`;
      node.style.width = `${(bar.end - bar.start + 1) * dayWidth}px`;
      node.style.top = `${index * ROW_HEIGHT}px`;
      if (bar.color) {
        node.style.background = bar.color.background;
        node.style.borderColor = bar.color.border;
      }
      node.title = bar.column ? `${bar.name} (${bar.column})` : bar.name;
      node.append(make('span', 'tlp-bar-name', bar.name));
      if (bar.column) node.append(make('span', 'tlp-bar-column', bar.column));
      if (canWrite()) {
        ['start', 'end'].forEach((edge) => {
          const handle = make('span', `tlp-handle tlp-handle-${edge}`);
          handle.dataset.edge = edge;
          node.append(handle);
        });
      }
      return node;
    };

    const dueNode = (bar, index, first) => {
      const dot = make('span', 'tlp-due');
      dot.style.left = `${(bar.due - first) * dayWidth + dayWidth / 2}px`;
      dot.style.top = `${index * ROW_HEIGHT + ROW_HEIGHT / 2}px`;
      dot.title = `Due ${MONTHS[dayDate(bar.due).getUTCMonth()]} ${dayDate(bar.due).getUTCDate()}`;
      return dot;
    };

    const render = () => {
      const today = localDay(new Date());
      const { first, last } = rangeOf(bars, today);
      const days = last - first + 1;
      viewFirst = first;
      const canvas = qs('tlp-canvas');
      canvas.replaceChildren();
      canvas.style.width = `${days * dayWidth}px`;
      canvas.append(monthRow(first, last), dayRow(first, last, today));

      const body = make('div', 'tlp-body');
      body.style.height = `${Math.max(bars.length, MIN_ROWS) * ROW_HEIGHT}px`;
      for (let day = first; day <= last; day++) {
        if (!isWeekend(day)) continue;
        const shade = make('div', 'tlp-weekend-shade');
        shade.style.left = `${(day - first) * dayWidth}px`;
        shade.style.width = `${dayWidth}px`;
        body.append(shade);
      }
      const line = make('div', 'tlp-today');
      line.style.left = `${(today - first) * dayWidth + dayWidth / 2 - 1}px`;
      body.append(line);
      bars.forEach((bar, index) => body.append(barNode(bar, index, first)));
      bars.forEach((bar, index) => {
        if (bar.due !== null) body.append(dueNode(bar, index, first));
      });
      canvas.append(body);

      qs('tlp-empty').hidden = bars.length > 0;
      setStatus(bars.length === 0 ? '' : `${bars.length} ${bars.length === 1 ? 'task' : 'tasks'} on the timeline.`);
    };

    const centreOn = (day) => {
      const scroll = qs('tlp-scroll');
      scroll.scrollLeft = (day - viewFirst) * dayWidth + dayWidth / 2 - scroll.clientWidth / 2;
    };
    const centreOnToday = () => centreOn(localDay(new Date()));
    const centreDay = () => {
      const scroll = qs('tlp-scroll');
      return viewFirst + (scroll.scrollLeft + scroll.clientWidth / 2) / dayWidth;
    };
    const clampZoom = (value) => Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, Math.round(Number(value)) || MIN_ZOOM));
    const setZoom = (value) => {
      const centre = centreDay();
      dayWidth = clampZoom(value);
      render();
      centreOn(centre);
    };

    const renderPanel = () => {
      const list = qs('tlp-list');
      if (!list) return;
      list.replaceChildren();
      columns.forEach((column) => {
        const needle = query.trim().toLowerCase();
        const members = tasks.filter((task) => task.column_id === column.id
          && task.name.toLowerCase().includes(needle));
        if (needle && members.length === 0) return;
        const head = make('button', 'tlp-group-head');
        head.type = 'button';
        head.dataset.columnId = column.id;
        head.setAttribute('aria-expanded', String(!folded.has(column.id)));
        head.append(make('span', 'tlp-group-name', column.name), make('span', 'tlp-group-count', String(members.length)));
        const rows = make('ul', 'tlp-rows');
        rows.hidden = folded.has(column.id);
        members.forEach((task) => {
          const item = make('li');
          const button = make('button', 'tlp-row', task.name);
          button.type = 'button';
          button.dataset.taskId = task.id;
          item.append(button);
          rows.append(item);
        });
        const group = make('section', 'tlp-group');
        group.append(head, rows);
        list.append(group);
      });
    };

    const load = () => fetch(`/api/boards/${encodeURIComponent(boardId)}/export-data`, {
      headers: { Accept: 'application/json' },
    }).then((response) => {
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      return response.json();
    }).then((data) => {
      columns = data.columns || [];
      tasks = cardsOf(data.tasks);
      bars = collectBars({ ...data, tasks });
      render();
      renderPanel();
      if (firstLoad) {
        firstLoad = false;
        centreOnToday();
      }
    }).catch((error) => setStatus(`Could not load task data: ${error.message}`));

    const save = (bar, range, focusId) => {
      const refocus = () => {
        const node = qs('tlp-canvas').querySelector(`.tlp-bar[data-task-id="${CSS.escape(focusId)}"]`);
        if (node) node.focus();
      };
      return api(`/api/tasks/${encodeURIComponent(bar.id)}`, 'PATCH', {
        timeline_start: isoOf(range.start),
        timeline_end: isoOf(range.end),
      }).then((response) => {
        if (!response.ok) throw new Error('save failed');
        const centre = centreDay();
        Object.assign(bar, range);
        bars.sort((a, b) => a.start - b.start || a.end - b.end || a.name.localeCompare(b.name));
        render();
        centreOn(centre);
      }).catch(() => {
        render();
        toast('Could not save the timeline.');
      }).finally(refocus);
    };

    let saving = false;
    let drag = null;
    let suppressClick = false;

    const commit = (bar, mode, delta, focusId) => {
      if (saving || delta === 0) return;
      saving = true;
      save(bar, shifted(bar, mode, delta), focusId).finally(() => { saving = false; });
    };

    const startDrag = (event) => {
      const node = event.target.closest('.tlp-bar');
      if (!node || !canWrite() || saving || event.button !== 0) return;
      const handle = event.target.closest('.tlp-handle');
      drag = {
        node,
        bar: bars.find((entry) => entry.id === node.dataset.taskId),
        mode: handle ? handle.dataset.edge : 'move',
        x: event.clientX,
        delta: 0,
        left: node.offsetLeft,
        width: node.offsetWidth,
      };
      suppressClick = false;
      node.setPointerCapture(event.pointerId);
    };

    const moveDrag = (event) => {
      if (!drag) return;
      const raw = event.clientX - drag.x;
      if (Math.abs(raw) > 3) suppressClick = true;
      drag.delta = clampDelta(drag.bar, drag.mode, Math.round(raw / dayWidth));
      const shift = drag.delta * dayWidth;
      drag.node.style.left = `${drag.mode === 'end' ? drag.left : drag.left + shift}px`;
      drag.node.style.width = `${drag.mode === 'move' ? drag.width : drag.mode === 'start' ? drag.width - shift : drag.width + shift}px`;
    };

    const cancelDrag = () => {
      if (!drag) return;
      drag = null;
      render();
    };

    const endDrag = () => {
      if (!drag) return;
      const { bar, mode, delta } = drag;
      drag = null;
      commit(bar, mode, delta, bar.id);
    };

    const keyBar = (event) => {
      const node = event.target.closest('.tlp-bar');
      if (!node) return;
      const arrow = { ArrowLeft: -1, ArrowRight: 1 }[event.key];
      if (arrow !== undefined && canWrite()) {
        event.preventDefault();
        const bar = bars.find((entry) => entry.id === node.dataset.taskId);
        const mode = !event.shiftKey ? 'move' : event.altKey ? 'start' : 'end';
        commit(bar, mode, arrow, bar.id);
        return;
      }
      openBar(event);
    };

    const openBar = (event) => {
      const bar = event.target.closest('.tlp-bar');
      if (!bar) return;
      if (event.type === 'keydown') {
        if (event.key !== 'Enter' && event.key !== ' ') return;
        event.preventDefault();
      }
      if (event.type === 'click' && suppressClick) {
        suppressClick = false;
        return;
      }
      openModal(bar.dataset.taskId);
    };

    const init = (id) => {
      if (initialized) return;
      initialized = true;
      boardId = id;
      const canvas = qs('tlp-canvas');
      canvas.addEventListener('click', openBar);
      canvas.addEventListener('keydown', keyBar);
      canvas.addEventListener('pointerdown', startDrag);
      canvas.addEventListener('pointermove', moveDrag);
      canvas.addEventListener('pointerup', endDrag);
      canvas.addEventListener('pointercancel', cancelDrag);
      qs('tlp-zoom').addEventListener('input', (event) => setZoom(event.target.value));
      qs('tlp-centre').addEventListener('click', centreOnToday);
      const list = qs('tlp-list');
      if (list) {
        list.addEventListener('click', (event) => {
          const head = event.target.closest('.tlp-group-head');
          if (head) {
            const id = head.dataset.columnId;
            if (!folded.delete(id)) folded.add(id);
            renderPanel();
            list.querySelector(`.tlp-group-head[data-column-id="${CSS.escape(id)}"]`)?.focus();
            return;
          }
          const row = event.target.closest('.tlp-row');
          if (row) TimelineDialog.open(row.dataset.taskId, load);
        });
        qs('tlp-search').addEventListener('input', (event) => {
          query = event.target.value;
          renderPanel();
        });
        qs('tlp-panel-toggle').addEventListener('click', (event) => {
          const body = qs('tlp-panel-body');
          body.hidden = !body.hidden;
          event.currentTarget.setAttribute('aria-expanded', String(!body.hidden));
        });
      }
      load();
    };

    return { init, rangeOf, isoDay, isoOf, shifted, clampZoom, cardsOf };
  })();

  var BoardHistoryPage = (function () {
    var initialized = false;
    var boardId = '';
    var usersFilled = false;
    var appliedQuery = '';
    var loadToken = 0;

    var MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun',
      'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

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
      var time = clockText(d);
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

    function localDay(text) {
      var m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(text || '');
      return m ? new Date(+m[1], +m[2] - 1, +m[3]) : null;
    }

    function periodRange(period, now, fromText, toText) {
      var today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
      var monday = new Date(today);
      monday.setDate(today.getDate() - ((today.getDay() + 6) % 7));
      var week = function (shift) {
        var from = new Date(monday); from.setDate(monday.getDate() + shift);
        var to = new Date(from); to.setDate(from.getDate() + 7);
        return { from: from, to: to };
      };
      var month = function (shift) {
        return {
          from: new Date(now.getFullYear(), now.getMonth() + shift, 1),
          to: new Date(now.getFullYear(), now.getMonth() + shift + 1, 1),
        };
      };
      if (period === 'this-week') return week(0);
      if (period === 'last-week') return week(-7);
      if (period === 'this-month') return month(0);
      if (period === 'last-month') return month(-1);
      if (period === 'custom') {
        var from = localDay(fromText);
        var to = localDay(toText);
        if (to) to.setDate(to.getDate() + 1);
        return { from: from, to: to };
      }
      return { from: null, to: null };
    }

    function queryString(values, now) {
      var range = periodRange(values.period, now, values.from, values.to);
      var pairs = [];
      function add(key, value) {
        if (value) pairs.push(key + '=' + encodeURIComponent(value));
      }
      add('event', values.event);
      add('property', values.event === 'changed' ? values.property : '');
      add('user', values.user);
      add('from', range.from ? range.from.toISOString() : '');
      add('to', range.to ? range.to.toISOString() : '');
      add('order', values.order);
      return pairs.join('&');
    }

    function chosen() {
      return {
        event: qs('history-event-filter').value,
        property: qs('history-property-filter').value,
        user: qs('history-user-filter').value,
        period: qs('history-period-filter').value,
        from: qs('history-from').value,
        to: qs('history-to').value,
        order: qs('history-order-filter').value,
      };
    }

    function headline(row) {
      var t = '<b>' + escapeHtml(row.task) + '</b>';
      if (row.kind === 'comment') {
        return '<b>' + escapeHtml(row.user) + '</b> added to task ' + t + ' with text';
      }
      return 'Task ' + t + ': ' + escapeHtml(row.event);
    }

    function renderEntry(row, open) {
      var ms = new Date(row.time).getTime();
      var html = '<div class="history-entry">';
      html += '<div class="history-head">';
      html += '<span class="user-avatar history-avatar" aria-hidden="true">' +
        escapeHtml(initials(row.user)) + '</span>';
      html += '<span class="history-actor">' + escapeHtml(row.user) + '</span>';
      html += '<span class="history-time">' + escapeHtml(fmtTime(ms)) + '</span>';
      html += '</div>';
      html += '<div class="history-text">' + headline(row) + '</div>';
      if (row.detail) {
        html += '<div class="history-comment">' + escapeHtml(row.detail) + '</div>';
      }
      if (row.changed) {
        html += '<details class="history-compare"' + (open ? ' open' : '') + '>' +
          '<summary>Compare values</summary>' +
          '<table class="bshell-table"><thead><tr><th>Property</th><th>Old value</th><th>New value</th></tr></thead>' +
          '<tbody><tr><th>' + escapeHtml(row.event) + '</th><td>' + escapeHtml(row.old_value) +
          '</td><td>' + escapeHtml(row.new_value) + '</td></tr></tbody></table></details>';
      }
      html += '</div>';
      return html;
    }

    function fillUsers(users) {
      if (usersFilled) return;
      usersFilled = true;
      var sel = qs('history-user-filter');
      users.forEach(function (u) {
        var o = document.createElement('option');
        o.value = u; o.textContent = u;
        sel.appendChild(o);
      });
    }

    function render(page) {
      fillUsers(page.users || []);
      var rows = page.rows || [];
      var feed = qs('history-feed');
      var open = qs('history-show-all').checked;
      feed.innerHTML = rows.map(function (r) { return renderEntry(r, open); }).join('');
      setStatus(rows.length === 0
        ? 'No changes match the selected filters.'
        : rows.length === 1 ? '1 change.' : rows.length + ' changes.');
    }

    function exportHref(format) {
      return '/api/boards/' + encodeURIComponent(boardId) + '/history?format=' + format +
        (appliedQuery ? '&' + appliedQuery : '');
    }

    function reload() {
      appliedQuery = queryString(chosen(), new Date());
      qs('history-export-xlsx').href = exportHref('xlsx');
      qs('history-export-csv').href = exportHref('csv');
      setStatus('Loading...');
      var token = ++loadToken;
      return fetch('/api/boards/' + encodeURIComponent(boardId) + '/history' +
        (appliedQuery ? '?' + appliedQuery : ''), {
        headers: { 'Accept': 'application/json' },
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (page) {
        if (token === loadToken) render(page);
      }).catch(function (err) {
        if (token === loadToken) setStatus('Could not load board history: ' + err.message);
      });
    }

    function syncVisibility() {
      qs('history-property-label').hidden = qs('history-event-filter').value !== 'changed';
      var custom = qs('history-period-filter').value === 'custom';
      document.querySelectorAll('.history-custom').forEach(function (el) { el.hidden = !custom; });
    }

    var FILTER_FIELDS = ['history-event-filter', 'history-property-filter', 'history-user-filter',
      'history-period-filter', 'history-from', 'history-to', 'history-order-filter'];

    var Bookmarks = {
      key: function () { return 'pomb-history-bookmarks-' + boardId; },
      read: function () {
        try { return JSON.parse(window.localStorage.getItem(this.key()) || '[]') || []; } catch (e) { return []; }
      },
      write: function (list) {
        try { window.localStorage.setItem(this.key(), JSON.stringify(list)); } catch (e) { toast('Could not save the bookmark.'); }
        this.render();
      },
      render: function () {
        var self = this;
        var list = this.read();
        qs('history-bookmarks-count').textContent = String(list.length);
        var ul = qs('history-bookmarks-list');
        ul.innerHTML = '';
        list.forEach(function (b, i) {
          var li = document.createElement('li');
          var apply = document.createElement('button');
          apply.type = 'button';
          apply.className = 'spent-bookmark-apply';
          apply.textContent = b.name;
          apply.addEventListener('click', function () { self.apply(b); });
          var del = document.createElement('button');
          del.type = 'button';
          del.className = 'spent-bookmark-delete';
          del.setAttribute('aria-label', 'Delete bookmark ' + b.name);
          del.textContent = '×';
          del.addEventListener('click', function () {
            var next = self.read();
            next.splice(i, 1);
            self.write(next);
          });
          li.appendChild(apply);
          li.appendChild(del);
          ul.appendChild(li);
        });
      },
      save: function () {
        var input = qs('history-bookmark-name');
        var name = input.value.trim();
        if (!name) { input.focus(); return; }
        var filters = {};
        FILTER_FIELDS.forEach(function (id) { filters[id] = qs(id).value; });
        var list = this.read();
        list.push({ name: name, filters: filters });
        input.value = '';
        this.write(list);
      },
      apply: function (bookmark) {
        FILTER_FIELDS.forEach(function (id) {
          if (bookmark.filters && id in bookmark.filters) qs(id).value = bookmark.filters[id];
        });
        syncVisibility();
        reload();
      },
      init: function () {
        var self = this;
        qs('history-bookmarks-toggle').addEventListener('click', function () {
          var body = qs('history-bookmarks-body');
          body.hidden = !body.hidden;
          qs('history-bookmarks-toggle').setAttribute('aria-expanded', String(!body.hidden));
        });
        qs('history-bookmark-save').addEventListener('click', function () { self.save(); });
        qs('history-bookmark-name').addEventListener('keydown', function (e) {
          if (e.key === 'Enter') { e.preventDefault(); self.save(); }
        });
        this.render();
      },
    };

    function init(id) {
      if (initialized) return;
      initialized = true;
      boardId = id;
      Bookmarks.init();
      qs('history-event-filter').addEventListener('change', syncVisibility);
      qs('history-period-filter').addEventListener('change', syncVisibility);
      qs('history-reload').addEventListener('click', reload);
      qs('history-show-all').addEventListener('change', function () {
        var open = qs('history-show-all').checked;
        document.querySelectorAll('#history-feed details').forEach(function (el) { el.open = open; });
      });
      syncVisibility();
      reload();
    }

    return { init: init, periodRange: periodRange, queryString: queryString };
  })();

  var PrintBoard = (function () {
    var PROPERTIES = [
      ['description', 'Description'], ['color', 'Color'], ['members', 'Members'],
      ['estimate', 'Time estimate'], ['spent', 'Time spent'], ['labels', 'Labels'],
      ['timeline', 'Timeline'], ['due', 'Due dates'], ['subtasks', 'Subtasks'],
      ['comments', 'Comments'], ['relations', 'Relations'], ['grouping', 'Grouping date'],
    ];
    var PERIODS = [
      ['all', 'All time'], ['this-week', 'This week'], ['last-week', 'Last week'],
      ['this-month', 'This month'], ['last-month', 'Last month'],
    ];
    var RELATION_LABELS = {
      blocked_by: 'Depends on', blocks: 'Required by', related: 'Relates to', parent: 'Part of',
    };
    var UNASSIGNED = '__unassigned__';

    var data = null;
    var wired = false;

    function qs(id) { return document.getElementById(id); }

    function pad2(n) { return (n < 10 ? '0' : '') + n; }

    function minutesText(minutes) {
      var m = Math.round(minutes);
      var h = Math.floor(m / 60);
      var rest = m % 60;
      if (h && rest) return h + 'h ' + rest + 'm';
      return h ? h + 'h' : rest + 'm';
    }

    function dayText(text) {
      if (/^\d{4}-\d{2}-\d{2}$/.test(text)) return text;
      var d = new Date(text);
      if (isNaN(d.getTime())) return String(text);
      return d.getFullYear() + '-' + pad2(d.getMonth() + 1) + '-' + pad2(d.getDate());
    }

    function lookup(list) {
      var map = {};
      (list || []).forEach(function (item) { map[item.id] = item; });
      return map;
    }

    function selected(select) {
      return Array.prototype.filter.call(select.options, function (o) { return o.selected; })
        .map(function (o) { return o.value; });
    }

    function selectTasks(board, filters, now) {
      var columns = board.columns || [];
      var order = {};
      var done = {};
      var lanes = {};
      (board.swimlanes || []).forEach(function (s, i) { lanes[s.id] = i; });
      columns.forEach(function (c, i) { order[c.id] = i; done[c.id] = !!c.is_done; });
      var range = BoardHistoryPage.periodRange(filters.period, now, '', '');
      function inPeriod(task) {
        if (!range.from && !range.to) return true;
        var at = new Date(task.completed_at || '').getTime();
        if (isNaN(at)) return false;
        return (!range.from || at >= range.from.getTime()) && (!range.to || at < range.to.getTime());
      }
      return (board.tasks || []).filter(function (t) {
        var members = t.member_ids || [];
        return filters.columns.indexOf(t.column_id) >= 0 &&
          (!filters.user || (filters.user === UNASSIGNED
            ? members.length === 0 : members.indexOf(filters.user) >= 0)) &&
          (!filters.color || t.color_id === filters.color) &&
          (!filters.label || (t.labels || []).indexOf(filters.label) >= 0) &&
          (!done[t.column_id] || inPeriod(t));
      }).sort(function (a, b) {
        return (order[a.column_id] - order[b.column_id]) ||
          ((lanes[a.swimlane_id] || 0) - (lanes[b.swimlane_id] || 0)) || (a.position - b.position);
      });
    }

    function rows(task, board, props) {
      var users = lookup(board.users);
      var colors = lookup(board.colors);
      var tasks = lookup(board.tasks);
      var out = [];
      function add(key, label, html) {
        if (props.indexOf(key) >= 0 && html) out.push([label, html]);
      }
      add('description', 'Description', escapeHtml(task.description || ''));
      var color = colors[task.color_id];
      add('color', 'Color', color ? escapeHtml(color.label || color.value) : '');
      add('members', 'Members', (task.member_ids || []).map(function (id) {
        return escapeHtml(users[id] ? users[id].username : id);
      }).join(', '));
      add('estimate', 'Time estimate',
        task.estimate_hours ? minutesText(task.estimate_hours * 60) : '');
      add('spent', 'Time spent', task.total_minutes ? minutesText(task.total_minutes) : '');
      add('labels', 'Labels', (task.labels || []).map(escapeHtml).join(', '));
      add('timeline', 'Timeline', task.timeline_start || task.timeline_end
        ? escapeHtml(dayText(task.timeline_start || task.timeline_end) + ' to ' +
          dayText(task.timeline_end || task.timeline_start)) : '');
      var dues = [];
      if (task.due_at) dues.push('Task: ' + dayText(task.due_at));
      (task.subtasks || []).forEach(function (s) {
        if (s.due_at) dues.push(s.name + ': ' + dayText(s.due_at));
      });
      add('due', 'Due dates', dues.map(escapeHtml).join('<br>'));
      add('subtasks', 'Subtasks', (task.subtasks || []).map(function (s) {
        return (s.done ? '&#9745; ' : '&#9744; ') + escapeHtml(s.name);
      }).join('<br>'));
      add('comments', 'Comments', (task.comments || []).map(function (c) {
        return '<b>' + escapeHtml(c.author) + '</b>: ' + escapeHtml(c.body);
      }).join('<br>'));
      add('relations', 'Relations', (task.links || []).map(function (l) {
        var other = tasks[l.target_id];
        return escapeHtml((RELATION_LABELS[l.kind] || l.kind) + ' ' +
          (other ? other.name : 'a task on another board'));
      }).join('<br>'));
      add('grouping', 'Grouping date', task.grouping_date ? escapeHtml(dayText(task.grouping_date)) : '');
      return out;
    }

    function blockHtml(task, board, props) {
      var columns = lookup(board.columns);
      var column = columns[task.column_id];
      var html = '<section class="print-task"><h3>' + escapeHtml(task.name) + '</h3>' +
        '<div class="print-task-column">' + escapeHtml(column ? column.name : '') + '</div>';
      var list = rows(task, board, props);
      if (list.length) {
        html += '<dl>' + list.map(function (r) {
          return '<dt>' + r[0] + '</dt><dd>' + r[1] + '</dd>';
        }).join('') + '</dl>';
      }
      return html + '</section>';
    }

    function sheetHtml(board, filters, now) {
      var tasks = selectTasks(board, filters, now);
      var head = '<h2>' + escapeHtml(board.board ? board.board.name : '') + '</h2>';
      if (!tasks.length) return head + '<p>No tasks match the filters.</p>';
      return head + tasks.map(function (t) {
        return blockHtml(t, board, filters.props);
      }).join('');
    }

    function chosen() {
      return {
        columns: selected(qs('print-board-columns')),
        props: selected(qs('print-board-props')),
        user: qs('print-board-user').value,
        color: qs('print-board-color').value,
        label: qs('print-board-label').value,
        period: qs('print-board-period').value,
      };
    }

    function render() {
      if (!data) return;
      qs('print-board-sheet').innerHTML = sheetHtml(data, chosen(), new Date());
    }

    function fill(select, items, all) {
      select.innerHTML = '';
      items.forEach(function (item) {
        var o = document.createElement('option');
        o.value = item[0];
        o.textContent = item[1];
        if (all) o.selected = true;
        select.appendChild(o);
      });
    }

    function populate(board) {
      fill(qs('print-board-columns'), (board.columns || []).map(function (c) {
        return [c.id, c.name];
      }), true);
      fill(qs('print-board-props'), PROPERTIES, true);
      fill(qs('print-board-user'), [['', 'All users'], [UNASSIGNED, 'Unassigned']].concat(
        (board.users || []).map(function (u) { return [u.id, u.username]; })), false);
      fill(qs('print-board-color'), [['', 'All colors']].concat(
        (board.colors || []).map(function (c) { return [c.id, c.label || c.value]; })), false);
      var labels = {};
      (board.tasks || []).forEach(function (t) {
        (t.labels || []).forEach(function (l) { labels[l] = true; });
      });
      fill(qs('print-board-label'), [['', 'All labels']].concat(
        Object.keys(labels).sort().map(function (l) { return [l, l]; })), false);
      fill(qs('print-board-period'), PERIODS, false);
    }

    function close() {
      qs('print-board-dialog').hidden = true;
      document.body.classList.remove('print-board-open');
    }

    function wire() {
      if (wired) return;
      wired = true;
      var overlay = qs('print-board-dialog');
      document.body.appendChild(overlay);
      ['columns', 'props', 'user', 'color', 'label', 'period'].forEach(function (name) {
        qs('print-board-' + name).addEventListener('change', render);
      });
      qs('print-board-filter').addEventListener('click', function () {
        var panel = qs('print-board-filters');
        panel.hidden = !panel.hidden;
        qs('print-board-filter').setAttribute('aria-expanded', String(!panel.hidden));
      });
      qs('print-board-now').addEventListener('click', function () { window.print(); });
      qs('print-board-close').addEventListener('click', close);
      overlay.addEventListener('click', function (e) { if (e.target === overlay) close(); });
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && !overlay.hidden) close();
      });
    }

    function open(boardId) {
      wire();
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' },
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (board) {
        data = board;
        populate(board);
        qs('print-board-filters').hidden = false;
        qs('print-board-dialog').hidden = false;
        document.body.classList.add('print-board-open');
        render();
      }).catch(function () { toast('Could not load the board to print.'); });
    }

    return { open: open, selectTasks: selectTasks, sheetHtml: sheetHtml };
  })();

  // Calendar report: tasks laid out by due date on a month grid, filterable by
  // user, color, labels and swimlane, in a dialog over the board.
  var CalendarPage = (function () {
    var initialized = false;

    var MONTHS = ['January', 'February', 'March', 'April', 'May', 'June',
      'July', 'August', 'September', 'October', 'November', 'December'];
    var WEEKDAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];

    var UNASSIGNED = '__unassigned__';
    var SETTINGS_KEY = 'pomb.calendar.settings';

    var firstDayOfWeek = accountPrefs.weekStart;
    var showRepeats = true;
    var currentBoard = null;
    var selectedBoards = [];
    var allBoards = [];
    var feeds = [];
    var loadToken = 0;
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
      var t = parseDueAt(s).getTime();
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


    function weekdayNames(firstDow) {
      var out = [];
      for (var i = 0; i < 7; i++) out.push(WEEKDAYS[(firstDow + i) % 7]);
      return out;
    }

    function gridRange(year, month, firstDow) {
      var start = new Date(year, month, 1);
      start.setDate(start.getDate() - ((start.getDay() - firstDow + 7) % 7));
      var end = new Date(year, month + 1, 0);
      end.setDate(end.getDate() + (6 - ((end.getDay() - firstDow + 7) % 7)));
      return { start: start, end: end };
    }

    function dayLabel(d) {
      return d.getDate() === 1 ? MONTHS[d.getMonth()] + ' 1' : String(d.getDate());
    }

    function parseMove(detail, currentName) {
      var body = detail.slice('Moved from '.length).split('; dropped fields:')[0];
      var tail = currentName ? ' to ' + currentName : null;
      if (tail && body.length > tail.length && body.slice(-tail.length) === tail) {
        return { from: body.slice(0, -tail.length), to: currentName };
      }
      var at = body.lastIndexOf(' to ');
      if (at < 0) return { from: null, to: null };
      return { from: body.slice(0, at), to: body.slice(at + 4) };
    }

    function moveLine(t, currentName) {
      var events = t.history || [];
      for (var i = events.length - 1; i >= 0; i--) {
        var e = events[i];
        if (e && e.kind === 'moved' && e.detail.indexOf('Moved from ') === 0) {
          var m = parseMove(e.detail, currentName);
          if (m.from === null) return currentName || '';
          return m.from + ' \u2192 ' + (currentName || m.to);
        }
      }
      return currentName || '';
    }

    function dayCellHtml(d, viewMon, todayK, chipsHtml) {
      var cls = 'cal-day';
      if (d.getMonth() !== viewMon) cls += ' cal-outside';
      var today = dayKey(d) === todayK;
      if (today) cls += ' cal-today';
      return '<div class="' + cls + '" role="gridcell">' +
        '<div class="cal-daynum">' +
        (today ? '<span class="cal-today-label">Today</span>' : '') +
        '<span class="cal-daynum-text">' + dayLabel(d) + '</span></div>' +
        (chipsHtml || '') + '</div>';
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

    function currentFilters() {
      return {
        user: qs('calendar-user-filter').value,
        color: qs('calendar-color-filter').value,
        label: qs('calendar-label-filter').value,
        swimlane: qs('calendar-swimlane-filter').value,
      };
    }

    function taskPasses(t, f) {
      var members = t.member_ids || [];
      if (f.user === UNASSIGNED) {
        if (members.length) return false;
      } else if (f.user && members.indexOf(f.user) < 0) {
        return false;
      }
      if (f.color && (t.color_id || '') !== f.color) return false;
      if (f.label && (t.labels || []).indexOf(f.label) < 0) return false;
      if (f.swimlane && (t.swimlane_id || '') !== f.swimlane) return false;
      return true;
    }

    function bucketTasks(tasks, start, end, repeats, f) {
      var byDay = {};
      var shown = 0;
      function addChip(t, day) {
        if (day < start || day > end) return;
        var k = dayKey(day);
        (byDay[k] = byDay[k] || []).push({ t: t, day: day });
        shown++;
      }
      tasks.forEach(function (t) {
        if (!t.due_at || !taskPasses(t, f)) return;
        var day = parseLocalDay(t.due_at);
        if (!day) return;
        addChip(t, day);
        if (!repeats) return;
        var instances = repeatInstances(day, t.due_repeat, start, end);
        for (var i = 0; i < instances.length; i++) addChip(t, instances[i]);
      });
      return { byDay: byDay, shown: shown };
    }

    function mergeBoards(parts) {
      var out = { tasks: [], columns: [], swimlanes: [], colors: [], users: [] };
      var seenUsers = {};
      parts.forEach(function (p) {
        var name = (p.data.board && p.data.board.name) || p.name || '';
        (p.data.tasks || []).forEach(function (t) {
          var copy = {};
          Object.keys(t).forEach(function (k) { copy[k] = t[k]; });
          copy.board_name = name;
          out.tasks.push(copy);
        });
        out.columns = out.columns.concat(p.data.columns || []);
        out.swimlanes = out.swimlanes.concat(p.data.swimlanes || []);
        out.colors = out.colors.concat(p.data.colors || []);
        (p.data.users || []).forEach(function (u) {
          if (seenUsers[u.id]) return;
          seenUsers[u.id] = true;
          out.users.push(u);
        });
      });
      return out;
    }

    function exportUrl(boardIds, opts) {
      var q = ['boards=' + encodeURIComponent(boardIds.join(','))];
      q.push('format=' + encodeURIComponent(opts.format || 'xlsx'));
      if (opts.from) q.push('from=' + encodeURIComponent(opts.from));
      if (opts.to) q.push('to=' + encodeURIComponent(opts.to));
      q.push('type=' + encodeURIComponent(opts.type || 'all'));
      return '/api/calendar-export?' + q.join('&');
    }

    function feedUrl(origin, path) { return origin + path; }

    function feedRowHtml(feed, origin) {
      var https = feedUrl(origin, feed.path);
      return '<li class="cal-feed-row" data-feed-id="' + escapeHtml(feed.id) + '">' +
        '<span class="cal-feed-name">' + escapeHtml(feed.label) + '</span>' +
        '<input class="cal-feed-url" readonly value="' + escapeHtml(https) +
        '" aria-label="URL of ' + escapeHtml(feed.label) + '">' +
        '<a class="cal-tool" href="' + escapeHtml(https.replace(/^https?:\/\//, 'webcal://')) +
        '">Subscribe</a>' +
        '<button type="button" class="cal-tool" data-feed-copy>Copy</button>' +
        '<button type="button" class="cal-icon" data-feed-delete aria-label="Delete ' +
        escapeHtml(feed.label) + '">&times;</button></li>';
    }

    function renderChip(t, day) {
      var bg = colorValueById(t.color_id) || '#e5e7eb';
      var fg = chipText(bg);
      var sub = moveLine(t, colNameById(t.column_id));
      if (selectedBoards.length > 1 && t.board_name) sub = t.board_name + ' · ' + sub;
      return '<div class="cal-chip" role="button" tabindex="0" data-task-id="' + escapeHtml(t.id) +
        '" style="background:' + escapeHtml(bg) +
        ';color:' + fg + '" title="' + escapeHtml(t.name) + '">' +
        '<div class="cal-chip-name">' + escapeHtml(t.name) + '</div>' +
        '<div class="cal-chip-sub">' + escapeHtml(sub) + '</div></div>';
    }

    function gridHtml(tasks, year, month, firstDow, repeats, filters, todayK) {
      var range = gridRange(year, month, firstDow);
      var bucket = bucketTasks(tasks, range.start, range.end, repeats, filters);
      var html = '';
      weekdayNames(firstDow).forEach(function (w) {
        html += '<div class="cal-dow" role="columnheader">' + w + '</div>';
      });
      for (var d = new Date(range.start); d <= range.end; d.setDate(d.getDate() + 1)) {
        var chips = '';
        (bucket.byDay[dayKey(d)] || []).forEach(function (item) {
          chips += renderChip(item.t, item.day);
        });
        html += dayCellHtml(d, month, todayK, chips);
      }
      return { html: html, shown: bucket.shown };
    }

    function render() {
      qs('calendar-title').textContent = MONTHS[viewMonth] + ' ' + viewYear;
      var grid = gridHtml(dataset.tasks || [], viewYear, viewMonth, firstDayOfWeek, showRepeats,
        currentFilters(), dayKey(new Date()));
      var shown = grid.shown;
      qs('calendar-grid').innerHTML = grid.html;
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
      function fill(id, map, labelById, keep) {
        var sel = qs(id);
        var held = sel.value;
        while (sel.options.length > keep) sel.remove(keep);
        Object.keys(map).sort().forEach(function (key) {
          var o = document.createElement('option');
          o.value = key;
          o.textContent = labelById(key) || key;
          sel.appendChild(o);
        });
        sel.value = held;
        if (sel.value !== held) sel.value = '';
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
      fill('calendar-user-filter', users, userName, 2);
      fill('calendar-color-filter', colors, colorLabel, 1);
      fill('calendar-label-filter', labels, function (l) { return l; }, 1);
      fill('calendar-swimlane-filter', swimlanes, namedLabel(dataset.swimlanes || []), 1);
    }

    function loadSettings() {
      try {
        var s = JSON.parse(window.localStorage.getItem(SETTINGS_KEY) || '{}');
        if (typeof s.showRepeats === 'boolean') showRepeats = s.showRepeats;
      } catch (e) {}
    }

    function saveSettings() {
      try {
        window.localStorage.setItem(SETTINGS_KEY,
          JSON.stringify({ showRepeats: showRepeats }));
      } catch (e) {}
    }

    var POPS = {
      'calendar-boards-toggle': 'calendar-boards-panel',
      'calendar-feed-toggle': 'calendar-feed-panel',
      'calendar-export': 'calendar-export-panel',
      'calendar-settings': 'calendar-settings-panel',
    };

    function closePops(except) {
      Object.keys(POPS).forEach(function (toggleId) {
        var panel = qs(POPS[toggleId]);
        var toggle = qs(toggleId);
        if (panel && POPS[toggleId] !== except) panel.hidden = true;
        if (toggle && POPS[toggleId] !== except) toggle.setAttribute('aria-expanded', 'false');
      });
    }

    function initPops(onOpen) {
      Object.keys(POPS).forEach(function (toggleId) {
        var toggle = qs(toggleId);
        var panel = qs(POPS[toggleId]);
        toggle.addEventListener('click', function (e) {
          e.stopPropagation();
          var opening = panel.hidden;
          closePops(POPS[toggleId]);
          panel.hidden = !opening;
          toggle.setAttribute('aria-expanded', opening ? 'true' : 'false');
          if (opening && onOpen[toggleId]) onOpen[toggleId]();
        });
      });
    }

    function feedStatus(msg) {
      var el = qs('calendar-feed-status');
      if (el) el.textContent = msg;
    }

    function feedsBase() {
      return '/api/boards/' + encodeURIComponent(currentBoard) + '/calendar-feeds';
    }

    function renderFeeds() {
      var list = qs('calendar-feed-list');
      list.innerHTML = feeds.length
        ? feeds.map(function (f) { return feedRowHtml(f, window.location.origin); }).join('')
        : '<li class="cal-hint">No feeds yet.</li>';
    }

    function loadFeeds() {
      feedStatus('Loading feeds…');
      var board = currentBoard;
      fetchJson(feedsBase()).then(function (data) {
        if (board !== currentBoard) return;
        feeds = data.feeds || [];
        renderFeeds();
        feedStatus('');
      }).catch(function (err) {
        feedStatus('Could not load feeds: ' + err.message);
      });
    }

    function initFeeds() {
      var label = qs('calendar-feed-label');
      qs('calendar-feed-generate').addEventListener('click', function () {
        var text = label.value.trim();
        if (!text) { feedStatus('Give the feed a label first.'); label.focus(); return; }
        feedStatus('Generating…');
        var board = currentBoard;
        fetch(feedsBase(), {
          method: 'POST',
          headers: { 'Accept': 'application/json', 'Content-Type': 'application/json' },
          body: JSON.stringify({ label: text }),
        }).then(function (r) {
          if (r.ok) return r.json();
          return r.json().catch(function () { return {}; }).then(function (b) { throw new Error(b.message || 'HTTP ' + r.status); });
        }).then(function (feed) {
          if (board !== currentBoard) return;
          feeds.push(feed);
          label.value = '';
          renderFeeds();
          feedStatus('Feed created.');
        }).catch(function (err) {
          if (board !== currentBoard) return;
          feedStatus('Could not create the feed: ' + err.message);
        });
      });
      qs('calendar-feed-list').addEventListener('click', function (e) {
        var row = e.target.closest ? e.target.closest('[data-feed-id]') : null;
        if (!row) return;
        var id = row.getAttribute('data-feed-id');
        var feed = feeds.filter(function (f) { return f.id === id; })[0];
        if (!feed) return;
        if (e.target.closest('[data-feed-copy]')) {
          var u = feedUrl(window.location.origin, feed.path);
          if (navigator.clipboard && navigator.clipboard.writeText) {
            navigator.clipboard.writeText(u).then(function () {
              feedStatus('Copied ' + feed.label + '.');
            }, function () { feedStatus(u); });
          } else {
            feedStatus(u);
          }
        } else if (e.target.closest('[data-feed-delete]')) {
          if (!window.confirm('Delete the feed "' + feed.label + '"? Its URL will stop working.')) return;
          var board = currentBoard;
          fetch(feedsBase() + '/' + encodeURIComponent(id), {
            method: 'DELETE',
            headers: { 'Accept': 'application/json' },
          }).then(function (r) {
            if (!r.ok) throw new Error('HTTP ' + r.status);
            if (board !== currentBoard) return;
            feeds = feeds.filter(function (f) { return f.id !== id; });
            renderFeeds();
            feedStatus('Feed deleted; its URL no longer works.');
          }).catch(function (err) {
            if (board !== currentBoard) return;
            feedStatus('Could not delete the feed: ' + err.message);
          });
        }
      });
    }

    function exportStatus(msg) {
      var el = qs('calendar-export-status');
      if (el) el.textContent = msg;
    }

    function initExport() {
      qs('calendar-export-run').addEventListener('click', function () {
        var opts = {
          format: qs('calendar-export-format').value,
          from: qs('calendar-export-from').value,
          to: qs('calendar-export-to').value,
          type: qs('calendar-export-type').value,
        };
        if (opts.from && opts.to && opts.to < opts.from) {
          exportStatus('To date is before From date.');
          return;
        }
        exportStatus('Exporting…');
        fetch(exportUrl(selectedBoards, opts), { credentials: 'same-origin' }).then(function (r) {
          if (!r.ok) throw new Error('HTTP ' + r.status);
          return r.blob();
        }).then(function (blob) {
          saveBlob('due-dates.' + opts.format, blob);
          exportStatus('');
        }).catch(function (err) {
          exportStatus('Could not export: ' + err.message);
        });
      });
    }

    function initSettings() {
      var day = qs('calendar-first-day');
      var repeats = qs('calendar-show-repeats');
      day.value = String(firstDayOfWeek);
      repeats.checked = showRepeats;
      day.addEventListener('change', function () {
        firstDayOfWeek = day.value === '1' ? 1 : 0;
        accountPrefs.weekStart = firstDayOfWeek;
        fetch('/api/settings', {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          credentials: 'same-origin',
          body: JSON.stringify({ week_start: firstDayOfWeek === 1 ? 'monday' : 'sunday' }),
        });
        if (dataset) render();
      });
      repeats.addEventListener('change', function () {
        showRepeats = repeats.checked;
        saveSettings();
        if (dataset) render();
      });
    }

    function renderBoardPicker() {
      var list = qs('calendar-boards-list');
      list.innerHTML = allBoards.map(function (b) {
        return '<label class="cal-board-row"><input type="checkbox" value="' + escapeHtml(b.id) + '"' +
          (selectedBoards.indexOf(b.id) >= 0 ? ' checked' : '') + '> ' + escapeHtml(b.name) + '</label>';
      }).join('');
    }

    function loadBoardList() {
      fetchJson('/api/boards').then(function (boards) {
        allBoards = boards.filter(function (b) { return !b.archived || b.id === currentBoard; });
        renderBoardPicker();
      }).catch(function () {
        allBoards = [{ id: currentBoard, name: 'This board' }];
        renderBoardPicker();
      });
    }

    function initBoardPicker() {
      qs('calendar-boards-list').addEventListener('change', function () {
        var ids = [];
        Array.prototype.forEach.call(
          qs('calendar-boards-list').querySelectorAll('input[type="checkbox"]'),
          function (box) { if (box.checked) ids.push(box.value); });
        if (!ids.length) ids = [currentBoard];
        selectedBoards = ids;
        renderBoardPicker();
        loadData();
      });
    }

    function bindOnce() {
      if (initialized) return;
      initialized = true;
      loadSettings();
      initPops({
        'calendar-feed-toggle': loadFeeds,
        'calendar-boards-toggle': loadBoardList,
      });
      initFeeds();
      initExport();
      initSettings();
      initBoardPicker();
      qs('calendar-grid').addEventListener('click', function (e) {
        var chip = e.target.closest('.cal-chip');
        if (chip) openModal(chip.dataset.taskId);
      });
      qs('calendar-grid').addEventListener('keydown', function (e) {
        var chip = e.target.closest('.cal-chip');
        if (chip && (e.key === 'Enter' || e.key === ' ')) {
          e.preventDefault();
          openModal(chip.dataset.taskId);
        }
      });
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
      ['calendar-user-filter', 'calendar-color-filter',
        'calendar-label-filter', 'calendar-swimlane-filter'].forEach(function (id) {
        qs(id).addEventListener('change', function () { if (dataset) render(); });
      });
      var filterToggle = qs('calendar-filter-toggle');
      filterToggle.addEventListener('click', function () {
        var panel = qs('calendar-filter-panel');
        panel.hidden = !panel.hidden;
        filterToggle.setAttribute('aria-expanded', panel.hidden ? 'false' : 'true');
      });
      var overlay = qs('calendar-dialog');
      overlay.addEventListener('click', function (e) {
        if (e.target === overlay) { overlay.hidden = true; return; }
        if (!e.target.closest || !e.target.closest('.cal-pop')) closePops('');
      });
    }

    function loadData() {
      var token = ++loadToken;
      setStatus('Loading…');
      Promise.all(selectedBoards.map(function (id) {
        return fetchJson('/api/boards/' + encodeURIComponent(id) + '/export-data');
      })).then(function (datas) {
        if (token !== loadToken) return;
        dataset = mergeBoards(datas.map(function (data) { return { data: data }; }));
        fillFilters();
        render();
      }).catch(function (err) {
        if (token !== loadToken) return;
        setStatus('Could not load calendar data: ' + err.message);
      });
    }

    function open(boardId) {
      bindOnce();
      currentBoard = boardId;
      selectedBoards = [boardId];
      feeds = [];
      allBoards = [];
      var now = new Date();
      viewYear = now.getFullYear();
      viewMonth = now.getMonth();
      dataset = null;
      qs('calendar-grid').innerHTML = '';
      qs('calendar-title').textContent = MONTHS[viewMonth] + ' ' + viewYear;
      qs('calendar-feed-list').innerHTML = '';
      closePops('');
      qs('calendar-dialog').hidden = false;
      loadData();
    }

    return {
      open: open,
      weekdayNames: weekdayNames,
      gridRange: gridRange,
      dayLabel: dayLabel,
      moveLine: moveLine,
      dayCellHtml: dayCellHtml,
      taskPasses: taskPasses,
      bucketTasks: bucketTasks,
      gridHtml: gridHtml,
      renderChip: renderChip,
      mergeBoards: mergeBoards,
      exportUrl: exportUrl,
      feedRowHtml: feedRowHtml,
    };
  })();

  // KF-342: Monte Carlo forecasting report page — predicts when N tasks will
  // be completed by running thousands of simulations sampled from the
  // board's historical daily throughput (arrivals into the target column,
  // from "moved" history events). Renders client-side from the shared
  // export-data dataset; pure inline SVG, no chart library.
  var MonteCarloPage = (function () {
    var initialized = false;

    var FULL_MONTHS = ['January', 'February', 'March', 'April', 'May', 'June',
      'July', 'August', 'September', 'October', 'November', 'December'];
    // KanbanFlow's forecast percentiles, with the reference screenshot's
    // line colors.
    var PCTS = [95, 85, 70, 50];
    var PCT_COLORS = { 95: '#1b5e20', 85: '#7cb342', 70: '#e6b800', 50: '#fb8c00' };
    var SIMULATIONS = 10000;
    var TODAY = 'Today';

    var lastData = null;   // export-data payload
    var lastForecast = null;
    var loadFailed = false;
    var hasRun = false;
    var COUNT_MODE = { FIXED: 'fixed', BOARD: 'board' };
    var countMode = COUNT_MODE.FIXED;
    var DEFAULT_RANGE_DAYS = 29;
    var RELATIVE_MONTHS = { '6m': 6, '12m': 12 };
    var fixedCount = null;
    var MIN_BASIS = 5;
    var DAY = 86400000;
    var MAX_CUSTOM_DAYS = 3660;
    var lastBasis = null;  // { starts, counts } for the current settings

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('mc-status');
      if (el) el.textContent = msg;
    }

    function fmtForecastDate(ms) {
      var d = new Date(ms);
      return FULL_MONTHS[d.getMonth()] + ' ' + d.getDate() + ', ' + d.getFullYear();
    }

    function todayLocal() {
      var d = new Date();
      return new Date(d.getFullYear(), d.getMonth(), d.getDate());
    }

    function startDateMs(type, value) {
      if (type !== 'date' || !value) return todayLocal().getTime();
      var parts = value.split('-');
      return new Date(+parts[0], +parts[1] - 1, +parts[2]).getTime();
    }

    function bindStartDate(input) {
      input.addEventListener('focus', function () {
        if (input.type === 'date') return;
        input.type = 'date';
        input.value = isoDate(todayLocal());
      });
      input.addEventListener('blur', function () {
        if (input.type !== 'date' || (input.value && input.value !== isoDate(todayLocal()))) return;
        input.type = 'text';
        input.value = TODAY;
      });
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
    function dailyThroughput(tasks, targetName, startMs, days) {
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
      var first = new Date(startMs);
      for (var j = 0; j < days; j++) {
        starts.push(new Date(first.getFullYear(), first.getMonth(), first.getDate() + j));
      }
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
        (nTasks === 1 ? ' task' : ' tasks') + ' starting ' + ReportChart.shortDate(new Date(startMs));
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
          '" text-anchor="middle" class="chart-tick">' + escapeHtml(ReportChart.shortDate(mdt2)) + '</text>';
      });
      for (b = 0; b < nDays; b++) {
        if (!buckets[b]) continue;
        var bh = (buckets[b] / max) * innerH;
        svg += '<rect x="' + x(b).toFixed(1) + '" y="' + (padT + innerH - bh).toFixed(1) +
          '" width="' + bw.toFixed(1) + '" height="' + bh.toFixed(1) +
          '" class="mc-bar"><title>' + buckets[b] +
          (buckets[b] === 1 ? ' simulation' : ' simulations') +
          ' complete ' + escapeHtml(ReportChart.longDate(new Date(minD + b * DAY))) + '</title></rect>';
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
            '" text-anchor="middle" class="chart-tick">' + escapeHtml(ReportChart.shortDate(starts[i])) + '</text>';
        }
      }
      var pts = [];
      for (j = 0; j < n; j++) pts.push(x(j).toFixed(1) + ',' + y(counts[j]).toFixed(1));
      svg += '<polyline points="' + pts.join(' ') + '" class="chart-line" fill="none"/>';
      svg += '</svg>';
      host.innerHTML = svg;
    }

    function taskMatches(t, f) {
      if (f.column) {
        if (t.column_id !== f.column) return false;
      } else if (f.openOnly && t.completed_at) {
        return false;
      }
      return ReportBreakdown.matches(t, f);
    }

    function boardCount(tasks, f) {
      var n = 0;
      tasks.forEach(function (t) {
        if (taskMatches(t, f)) n++;
      });
      return n;
    }

    function parseDay(str) {
      var ms = ReportBreakdown.parseDay(str);
      return ms === null ? null : new Date(ms);
    }

    function periodRange(value, fromStr, toStr, now) {
      var today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
      var start = new Date(today);
      if (value === 'custom') {
        start = parseDay(fromStr);
        var end = parseDay(toStr);
        if (!start || !end || end < start) return null;
        if (end > today) end = today;
        if (end < start) return null;
        var span = Math.round((end - start) / DAY) + 1;
        return span > MAX_CUSTOM_DAYS ? null : { start: start.getTime(), days: span };
      }
      if (RELATIVE_MONTHS[value]) {
        var target = start.getMonth() - RELATIVE_MONTHS[value];
        start.setDate(1);
        start.setMonth(target);
        start.setDate(Math.min(today.getDate(), new Date(start.getFullYear(), start.getMonth() + 1, 0).getDate()));
      } else {
        start.setDate(start.getDate() - (parseInt(value, 10) - 1));
      }
      return { start: start.getTime(), days: Math.round((today - start) / DAY) + 1 };
    }

    function forecastRows(fc) {
      var rows = [['Probability', 'Completion date']];
      PCTS.forEach(function (p) {
        rows.push([p + '%', isoDate(new Date(percentile(fc.sorted, p)))]);
      });
      rows.push([], ['Tasks', fc.nTasks], ['Start date', isoDate(new Date(fc.startMs))],
        ['Simulations', fc.sims], ['Completed tasks in the period', fc.basisTotal]);
      return rows;
    }

    function forecastLines(fc) {
      var lines = ['Forecast to complete ' + fc.nTasks + (fc.nTasks === 1 ? ' task' : ' tasks') +
        ' starting ' + fmtForecastDate(fc.startMs), ''];
      PCTS.forEach(function (p) {
        lines.push(p + '% probability: ' + fmtForecastDate(percentile(fc.sorted, p)));
      });
      lines.push('', 'Based on ' + fc.basisTotal + ' completed tasks in the selected period (' +
        fc.sims + ' simulations).');
      return lines;
    }

    function notEnoughMessage(total) {
      return 'A forecast needs at least ' + MIN_BASIS + ' completed tasks in the selected period, and there ' +
        (total === 1 ? 'is 1.' : 'are ' + total + '.');
    }

    function targetColumnName() {
      var sel = qs('mc-target-column');
      if (!sel || !sel.options.length) return '';
      return sel.options[sel.selectedIndex].text;
    }

    function basisFilter() {
      return { user: qs('mc-user').value, color: qs('mc-color').value, label: qs('mc-label').value };
    }

    function countFilter() {
      return {
        column: qs('mc-count-column').value,
        openOnly: true,
        user: qs('mc-count-user').value,
        color: qs('mc-count-color').value,
        label: qs('mc-count-label').value,
      };
    }

    function currentBoardCount() {
      return boardCount(lastData ? lastData.tasks || [] : [], countFilter());
    }

    function resolvedTaskCount() {
      return countMode === COUNT_MODE.BOARD ? currentBoardCount() : fixedCount;
    }

    function showCount() {
      var n = currentBoardCount();
      qs('mc-count-current').textContent = n;
      var shown = countMode === COUNT_MODE.BOARD ? n + (n === 1 ? ' task on board' : ' tasks on board')
        : (fixedCount === null ? '' : String(fixedCount));
      qs('mc-task-count').textContent = shown || 'Set task count';
    }

    function refreshBasis() {
      if (!lastData || !hasRun) return true;
      var range = periodRange(qs('mc-period').value, qs('mc-from').value, qs('mc-to').value, new Date());
      if (!range) {
        setStatus('Pick a start date that is on or before the end date, within ten years.');
        return false;
      }
      var f = basisFilter();
      var tasks = (lastData.tasks || []).filter(function (t) { return taskMatches(t, f); });
      lastBasis = dailyThroughput(tasks, targetColumnName(), range.start, range.days);
      renderBasis(lastBasis);
      return true;
    }

    function clearForecast() {
      lastForecast = null;
      qs('mc-headline').textContent = '';
      qs('mc-forecast-lines').innerHTML = '';
      qs('mc-chart').innerHTML = '';
      qs('mc-export').hidden = true;
    }

    function runForecast() {
      if (!lastData) {
        if (!loadFailed) setStatus('Loading board data...');
        return;
      }
      var nTasks = resolvedTaskCount();
      if (!nTasks || nTasks < 1) {
        clearForecast();
        setStatus(countMode === COUNT_MODE.BOARD
          ? 'No tasks on the board match the task count filters.'
          : 'Enter how many tasks to forecast.');
        return;
      }
      var startInput = qs('mc-start-date');
      var startMs = startDateMs(startInput.type, startInput.value);
      hasRun = true;
      qs('mc-getting-started').hidden = true;
      qs('mc-chart-title').hidden = false;
      qs('mc-basis-title').hidden = false;
      if (!refreshBasis()) {
        clearForecast();
        return;
      }
      var total = lastBasis.counts.reduce(function (a, b) { return a + b; }, 0);
      if (total < MIN_BASIS) {
        clearForecast();
        qs('mc-headline').textContent = 'Not enough throughput data';
        qs('mc-forecast-lines').textContent = notEnoughMessage(total);
        setStatus('The forecast is not shown: ' + notEnoughMessage(total));
        return;
      }
      var sorted = runSimulation(lastBasis.counts, nTasks, SIMULATIONS, startMs);
      lastForecast = { nTasks: nTasks, startMs: startMs, sorted: sorted, sims: SIMULATIONS, basisTotal: total };
      renderForecast(nTasks, startMs, sorted);
      qs('mc-export').hidden = false;
      setStatus('Forecast based on ' + total + ' completed tasks in the selected period (' + SIMULATIONS + ' simulations).');
    }

    function exportPdf() {
      if (!lastForecast) return;
      saveBlob('monte-carlo-forecast.pdf', new Blob([ReportFile.pdfBytes('Monte Carlo forecast', forecastLines(lastForecast))],
        { type: 'application/pdf' }));
    }

    function exportXlsx() {
      if (!lastForecast) return;
      saveBlob('monte-carlo-forecast.xlsx', new Blob([ReportFile.xlsxBytes(forecastRows(lastForecast), 'Forecast')],
        { type: ReportFile.XLSX_MIME }));
    }

    function fillColumns(select, columns) {
      ReportBreakdown.fill(select, columns.map(function (c) { return { value: c.id, text: c.name }; }));
    }

    function bindCountPopover() {
      var pop = qs('mc-count-pop');
      var input = qs('mc-task-count');
      function close() { pop.hidden = true; }
      function toggle() { pop.hidden = !pop.hidden; }
      input.addEventListener('click', toggle);
      input.addEventListener('keydown', function (e) {
        if (e.key === 'Escape') close();
      });
      pop.addEventListener('keydown', function (e) {
        if (e.key === 'Escape') {
          close();
          input.focus();
        }
      });
      qs('mc-count-done').addEventListener('click', close);
      document.addEventListener('click', function (e) {
        if (!pop.hidden && !pop.contains(e.target) && !input.parentNode.contains(e.target)) close();
      });
      qs('mc-count-fixed').addEventListener('input', function () {
        var n = parseInt(this.value, 10);
        fixedCount = isNaN(n) ? null : n;
        showCount();
      });
      ['mc-count-mode-fixed', 'mc-count-mode-board'].forEach(function (id) {
        qs(id).addEventListener('change', function () {
          countMode = qs('mc-count-mode-board').checked ? COUNT_MODE.BOARD : COUNT_MODE.FIXED;
          showCount();
        });
      });
      ['mc-count-column', 'mc-count-user', 'mc-count-color', 'mc-count-label'].forEach(function (id) {
        qs(id).addEventListener('change', showCount);
      });
    }

    function init(boardId, doneColumnId) {
      if (initialized) return;
      initialized = true;
      bindStartDate(qs('mc-start-date'));
      bindCountPopover();
      qs('mc-run').addEventListener('click', runForecast);
      var today = todayLocal();
      var from = new Date(today);
      from.setDate(from.getDate() - DEFAULT_RANGE_DAYS);
      qs('mc-from').value = isoDate(from);
      qs('mc-to').value = isoDate(today);
      qs('mc-period').addEventListener('change', function () {
        qs('mc-custom').hidden = qs('mc-period').value !== 'custom';
        refreshBasis();
      });
      ['mc-target-column', 'mc-from', 'mc-to', 'mc-user', 'mc-color', 'mc-label'].forEach(function (id) {
        qs(id).addEventListener('change', refreshBasis);
      });
      var more = qs('mc-more'), moreWrap = qs('mc-more-wrap');
      more.addEventListener('change', function () {
        moreWrap.hidden = !more.checked;
      });
      qs('mc-export-pdf').addEventListener('click', exportPdf);
      qs('mc-export-xlsx').addEventListener('click', exportXlsx);
      ReportTools.mount({
        report: 'monte-carlo', title: 'Forecast', boardId: boardId, chartId: 'mc-chart',
        getFilters: function () { return {}; }, applyFilters: function () {},
        rows: function () { return lastForecast ? forecastRows(lastForecast) : []; },
      });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' },
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        lastData = data;
        var sel = qs('mc-target-column');
        var cols = data.columns || [];
        fillColumns(sel, cols);
        var defId = doneColumnId || (cols.length ? cols[cols.length - 1].id : '');
        if (defId) sel.value = defId;
        ReportBreakdown.fillFilters(data, 'mc-');
        ReportBreakdown.fillFilters(data, 'mc-count-');
        fillColumns(qs('mc-count-column'), cols);
        showCount();
      }).catch(function (err) {
        loadFailed = true;
        setStatus('Could not load forecast data: ' + err.message);
      });
    }

    return {
      init: init, startDateMs: startDateMs, bindStartDate: bindStartDate, taskMatches: taskMatches,
      boardCount: boardCount, periodRange: periodRange, forecastRows: forecastRows,
      forecastLines: forecastLines, dailyThroughput: dailyThroughput, notEnoughMessage: notEnoughMessage,
      PCTS: PCTS, PCT_COLORS: PCT_COLORS, SIMULATIONS: SIMULATIONS,
    };
  })();

  // KF-345: Time estimate report page — estimated workload by color and
  // column, summed from each task's time estimate (KanbanFlow's Time
  // estimate report: "an overview of the estimated workload on your
  // board"). Renders client-side from the shared export-data dataset.
  var TimeEstimatePage = (function () {
    var initialized = false;
    var lastTable = null;

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

    function estimateOf(t) {
      var est = parseFloat(t.estimate_hours);
      return !isNaN(est) && est > 0 ? est : 0;
    }

    function exportRows(table, seconds) {
      var rows = [[ReportBreakdown.GROUPS[table.group]].concat(table.columns.map(function (c) { return c.name; }))];
      table.rows.forEach(function (r) {
        rows.push([r.label].concat(r.sums.map(function (h) {
          return seconds ? Math.round(h * 3600) : fmtEstimate(h);
        })));
      });
      return rows;
    }

    function exportExcel(seconds) {
      if (!lastTable) return;
      var bytes = ReportFile.xlsxBytes(exportRows(lastTable, seconds), 'Time estimate');
      saveBlob(seconds ? 'time-estimate-total-seconds.xlsx' : 'time-estimate-human-readable.xlsx',
        new Blob([bytes], { type: ReportFile.XLSX_MIME }));
    }

    function render(data, opts) {
      if (!opts.range) {
        lastTable = null;
        setStatus('Pick a start date that is on or before the end date.');
        return;
      }
      var table = ReportBreakdown.build(data, opts, estimateOf);
      lastTable = table;
      qs('time-estimate-table').innerHTML = ReportBreakdown.tableHtml(table, fmtEstimate);
      setStatus(table.matched === 0
        ? 'No time estimates match.'
        : 'Total estimated workload: ' + fmtEstimate(table.total) + ' across ' +
          table.matched + (table.matched === 1 ? ' task' : ' tasks') + ' with estimates.');
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      qs('te-export-human').addEventListener('click', function () { exportExcel(false); });
      qs('te-export-seconds').addEventListener('click', function () { exportExcel(true); });
      ReportBreakdown.mount(boardId, {
        render: render,
        error: function (err) { setStatus('Could not load time estimate data: ' + err.message); },
      });
    }

    return { init: init, exportRows: exportRows, fmtEstimate: fmtEstimate };
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

  var TimeInColumnPage = (function () {
    var initialized = false;
    var MIN_MS = 60000;
    var DAY_MIN = 1440;
    var lastTable = null;
    var lastNow = 0;

    function qs(id) { return document.getElementById(id); }

    function setStatus(msg) {
      var el = qs('tic-status');
      if (el) el.textContent = msg;
    }

    function pad(n) { return (n < 10 ? '0' : '') + n; }

    function fmtDuration(ms, unit) {
      var step = unit === 'days' ? DAY_MIN : unit === 'hours' ? 60 : 1;
      var total = Math.round(ms / MIN_MS / step) * step;
      var d = Math.floor(total / DAY_MIN);
      var h = Math.floor((total % DAY_MIN) / 60);
      var m = total % 60;
      if (unit === 'days') return d + 'd';
      if (unit === 'hours') return d > 0 ? d + 'd ' + pad(h) + 'h' : h + 'h';
      if (d > 0) return d + 'd ' + pad(h) + 'h ' + pad(m) + 'm';
      if (h > 0) return h + 'h ' + pad(m) + 'm';
      return m + 'm';
    }

    function destination(e) {
      var detail = e.detail || '';
      if (e.kind === 'created' && detail.indexOf('Task created in ') === 0) {
        return detail.slice('Task created in '.length);
      }
      if (e.kind === 'moved' && detail.indexOf('Moved from ') === 0) {
        var body = detail.slice('Moved from '.length);
        var sep = ' to ';
        var i = body.lastIndexOf(sep);
        return i === -1 ? null : body.slice(i + sep.length);
      }
      return null;
    }

    function columnIdByName(columns) {
      var byName = {};
      columns.forEach(function (c) { byName[c.name] = c.id; });
      columns.forEach(function (c) {
        (c.former_names || []).forEach(function (old) {
          if (byName[old] === undefined) byName[old] = c.id;
        });
      });
      return byName;
    }

    function timeline(task, current, byName) {
      var tl = [];
      (task.history || []).forEach(function (e) {
        var name = destination(e);
        var at = Date.parse(e.created_at);
        if (name == null || isNaN(at)) return;
        var col = e.column_id || byName[name];
        if (col !== undefined) tl.push({ at: at, col: col });
      });
      tl.sort(function (a, b) { return a.at - b.at; });
      var created = Date.parse(task.created_at);
      if (tl.length === 0 && !isNaN(created)) tl.push({ at: created, col: task.column_id });
      if (tl.length > 0 && current[task.column_id]) tl[tl.length - 1].col = task.column_id;
      return tl;
    }

    function intervals(task, current, byName, now, from) {
      var tl = timeline(task, current, byName);
      var out = [];
      tl.forEach(function (p, i) {
        var end = Math.min(i + 1 < tl.length ? tl[i + 1].at : now, now);
        var start = Math.max(p.at, from);
        if (end - start > 0 && current[p.col]) out.push({ col: p.col, start: start, end: end, open: i + 1 >= tl.length });
      });
      return out;
    }

    function taskRows(data, task, now) {
      var columns = data.columns || [];
      var current = {};
      columns.forEach(function (c) { current[c.id] = true; });
      var list = intervals(task, current, columnIdByName(columns), now, -Infinity);
      var rows = [];
      columns.forEach(function (c) {
        var mine = list.filter(function (iv) { return iv.col === c.id; });
        if (!mine.length) return;
        var ms = 0;
        mine.forEach(function (iv) { ms += iv.end - iv.start; });
        rows.push({ id: c.id, name: c.name, ms: ms, intervals: mine });
      });
      return rows;
    }

    function buildTable(data, opts, now) {
      var allColumns = data.columns || [];
      var current = {};
      allColumns.forEach(function (c) { current[c.id] = true; });
      var byName = columnIdByName(allColumns);
      var shown = allColumns.filter(function (c) {
        return !opts.columns || opts.columns.indexOf(c.id) !== -1;
      });
      var from = opts.periodDays ? now - opts.periodDays * DAY_MIN * MIN_MS : -Infinity;
      var rows = [];
      (data.tasks || []).forEach(function (t) {
        if (opts.user && (t.member_ids || []).indexOf(opts.user) === -1) return;
        if (opts.color === 'none' ? t.color_id : (opts.color && t.color_id !== opts.color)) return;
        if (opts.label && (t.labels || []).indexOf(opts.label) === -1) return;
        var ms = {};
        var periods = {};
        intervals(t, current, byName, now, from).forEach(function (iv) {
          ms[iv.col] = (ms[iv.col] || 0) + (iv.end - iv.start);
          (periods[iv.col] = periods[iv.col] || []).push(iv);
        });
        if (!shown.some(function (c) { return ms[c.id]; })) return;
        rows.push({ id: t.id, name: t.name, ms: ms, periods: periods });
      });
      return { columns: shown, rows: rows };
    }

    function render(table, unit) {
      var host = qs('tic-table');
      if (!host) return;
      if (!table.rows.length) {
        host.innerHTML = '<p class="settings-hint">No tasks match the filters.</p>';
        return;
      }
      var html = '<table class="bshell-table"><thead><tr><th scope="col">Task</th>';
      table.columns.forEach(function (c) {
        html += '<th scope="col" class="num">' + escapeHtml(c.name) + '</th>';
      });
      html += '</tr></thead><tbody>';
      table.rows.forEach(function (r) {
        html += '<tr class="tic-row" tabindex="0" data-task-id="' + escapeHtml(r.id) + '"><th scope="row">' +
          escapeHtml(r.name) + '</th>';
        table.columns.forEach(function (c) {
          html += '<td class="num">' + escapeHtml(fmtDuration(r.ms[c.id] || 0, unit)) + '</td>';
        });
        html += '</tr>';
      });
      html += '</tbody></table>';
      host.innerHTML = html;
    }

    function attachRows(host, onOpen) {
      function rowOf(e) {
        return e.target && e.target.closest ? e.target.closest('tr[data-task-id]') : null;
      }
      host.addEventListener('click', function (e) {
        var row = rowOf(e);
        if (row) onOpen(row.dataset.taskId);
      });
      host.addEventListener('keydown', function (e) {
        var row = rowOf(e);
        if (row && e.key === 'Enter') {
          e.preventDefault();
          onOpen(row.dataset.taskId);
        }
      });
    }

    var KIND_HUMAN = 'human';
    var KIND_SECONDS = 'seconds';
    var KIND_PERIODS = 'periods';
    var FORMAT_XLSX = 'xlsx';
    var FORMAT_CSV = 'csv';
    var EXPORT_KINDS = {};
    EXPORT_KINDS[KIND_HUMAN] = 'Time human readable';
    EXPORT_KINDS[KIND_SECONDS] = 'Time total seconds';
    EXPORT_KINDS[KIND_PERIODS] = 'Periods';
    var EXPORT_FORMATS = {};
    EXPORT_FORMATS[FORMAT_XLSX] = 'Excel';
    EXPORT_FORMATS[FORMAT_CSV] = 'CSV';

    function optionsHtml(labels) {
      return Object.keys(labels).map(function (key) {
        return '<option value="' + key + '">' + labels[key] + '</option>';
      }).join('');
    }

    function exportKindOptions() { return optionsHtml(EXPORT_KINDS); }

    function exportFormatOptions() { return optionsHtml(EXPORT_FORMATS); }

    function isoOf(ms) { return new Date(ms).toISOString(); }

    function periodsText(list, now) {
      return (list || []).map(function (iv) {
        return isoOf(iv.start) + ' - ' + isoOf(iv.open ? now : iv.end);
      }).join('; ');
    }

    function exportCell(kind, ms, list, unit, now) {
      if (kind === KIND_SECONDS) return Math.round(ms / 1000);
      if (kind === KIND_PERIODS) return periodsText(list, now);
      return fmtDuration(ms, unit);
    }

    function boardExportRows(table, kind, unit, now) {
      var rows = [['Task'].concat(table.columns.map(function (c) { return c.name; }))];
      table.rows.forEach(function (r) {
        rows.push([r.name].concat(table.columns.map(function (c) {
          return exportCell(kind, r.ms[c.id] || 0, r.periods[c.id], unit, now);
        })));
      });
      return rows;
    }

    function taskExportRows(rows, kind, now) {
      var out = [['Column', EXPORT_KINDS[kind] || EXPORT_KINDS[KIND_HUMAN]]];
      rows.forEach(function (r) {
        out.push([r.name, exportCell(kind, r.ms, r.intervals, 'minutes', now)]);
      });
      return out;
    }

    function csvBytes(rows) {
      var text = rows.map(function (r) { return r.map(csvCell).join(','); }).join('\r\n');
      return new TextEncoder().encode('﻿' + text);
    }

    function exportFile(rows, format, stem) {
      return format === FORMAT_XLSX
        ? { name: stem + '.' + FORMAT_XLSX, bytes: ReportFile.xlsxBytes(rows, 'Time in column'), mime: ReportFile.XLSX_MIME }
        : { name: stem + '.' + FORMAT_CSV, bytes: csvBytes(rows), mime: 'text/csv;charset=utf-8' };
    }

    function exportStem(prefix, kind) {
      return prefix + '-' + (EXPORT_KINDS[kind] ? kind : KIND_HUMAN);
    }

    function saveExport(file) {
      saveBlob(file.name, new Blob([file.bytes], { type: file.mime }));
    }

    function fill(select, items) {
      items.forEach(function (it) {
        var o = document.createElement('option');
        o.value = it.value;
        o.textContent = it.text;
        select.appendChild(o);
      });
    }

    function selectedColumns() {
      var sel = qs('tic-columns');
      var ids = [];
      Array.prototype.forEach.call(sel.options, function (o) {
        if (o.selected) ids.push(o.value);
      });
      return ids.length === 0 || ids.length === sel.options.length ? null : ids;
    }

    function init(boardId) {
      if (initialized) return;
      initialized = true;
      try {
        if (window.localStorage.getItem('pomb-dark') === '1') document.body.classList.add('dark');
      } catch (e) {}
      qs('tic-filters').addEventListener('submit', function (e) { e.preventDefault(); });
      attachRows(qs('tic-table'), openModal);
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/export-data', {
        headers: { 'Accept': 'application/json' },
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        fill(qs('tic-columns'), (data.columns || []).map(function (c) {
          return { value: c.id, text: c.name };
        }));
        fill(qs('tic-user'), (data.users || []).map(function (u) {
          return { value: u.id, text: u.username };
        }));
        fill(qs('tic-color'), [{ value: 'none', text: 'No color' }].concat(
          (data.colors || []).map(function (c) {
            return { value: c.id, text: c.label || c.value };
          })));
        var labels = {};
        (data.tasks || []).forEach(function (t) {
          (t.labels || []).forEach(function (l) { labels[l] = true; });
        });
        fill(qs('tic-label'), Object.keys(labels).sort().map(function (l) {
          return { value: l, text: l };
        }));
        function refresh() {
          var period = qs('tic-period').value;
          var now = Date.now();
          var table = buildTable(data, {
            columns: selectedColumns(),
            periodDays: period === 'all' ? null : parseInt(period, 10),
            user: qs('tic-user').value,
            color: qs('tic-color').value,
            label: qs('tic-label').value,
          }, now);
          lastTable = table;
          lastNow = now;
          render(table, qs('tic-format').value);
          var n = table.rows.length;
          setStatus(n + (n === 1 ? ' task' : ' tasks') + ' shown.');
        }
        ['tic-columns', 'tic-period', 'tic-user', 'tic-color', 'tic-label', 'tic-format']
          .forEach(function (id) { qs(id).addEventListener('change', refresh); });
        qs('tic-export-kind').innerHTML = exportKindOptions();
        qs('tic-export-format').innerHTML = exportFormatOptions();
        qs('tic-export').addEventListener('click', function () {
          var kind = qs('tic-export-kind').value;
          saveExport(exportFile(
            boardExportRows(lastTable, kind, qs('tic-format').value, lastNow),
            qs('tic-export-format').value,
            exportStem('time-in-column', kind)));
        });
        refresh();
      }).catch(function (err) {
        setStatus('Could not load time in column data: ' + err.message);
      });
    }

    return {
      init: init, buildTable: buildTable, taskRows: taskRows, fmtDuration: fmtDuration,
      render: render, attachRows: attachRows,
      boardExportRows: boardExportRows, taskExportRows: taskExportRows, exportFile: exportFile,
      exportStem: exportStem, saveExport: saveExport,
      exportKindOptions: exportKindOptions, exportFormatOptions: exportFormatOptions,
    };
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
    var ticks = Math.min(4, max);
    var step = Math.max(1, Math.ceil(40 / slot));
    for (var g = 0; g <= ticks; g++) {
      var val = Math.round(max * g / ticks);
      var y = padT + innerH - (innerH * g / ticks);
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
        '" text-anchor="middle" font-size="11" fill="#777">' + b.count + '</text>';
      if (i % step === 0) {
        html += '<text x="' + cx + '" y="' + (H - 10) +
          '" text-anchor="middle" font-size="11" fill="#777">' + escapeHtml(b.label) + '</text>';
      }
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

  var MENU_KEEPS_OPEN = { dark: true, legend: true, 'large-names': true };

  var BoardChrome = {
    initialized: false,
    LABEL_MODE: { AND: 'and', OR: 'or' },
    FOLD_MODE: { COLLAPSE: 'collapse', HIDE: 'hide' },
    OPEN_DWELL_MS: 400,
    CLOSE_GRACE_MS: 150,
    closeBoardMenu: null,

    init: function () {
      if (this.initialized) return;
      this.initialized = true;
      if (document.querySelector('.board-wrap') || document.querySelector('.login-page')) this.restorePrefs();
      if (!document.querySelector('.board-wrap')) return;
      this.initMenu();
      this.initLegend();
      this.initFilter();
      this.initEpics();
      this.initBarButtons();
      if (window.location.hash === '#layout' && document.querySelector('.board-wrap[data-role="owner"]')) this.editLayout();
      this.initEsc();
      this.initHelp();
    },

    // ----- preferences -----
    restorePrefs: function () {
      try {
        if (window.localStorage.getItem('pomb-dark') === '1') document.body.classList.add('dark');
        if (window.localStorage.getItem('pomb-large-names') === '1') document.body.classList.add('large-names');
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

      // KF-367: the panel runs from under the top bar to the viewport bottom.
      function placeMenu() {
        var bar = document.querySelector('.topbar');
        var top = (bar ? bar.getBoundingClientRect().bottom : 0) + 'px';
        menu.style.top = top;
        sub.style.top = top;
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
      document.getElementById('reports-close').addEventListener('click', function (e) {
        e.stopPropagation();
        closeMenu();
      });
      document.getElementById('reports-back').addEventListener('click', function (e) {
        e.stopPropagation();
        closeSub();
        menu.hidden = false;
      });
      document.getElementById('board-menu-close').addEventListener('click', function (e) {
        e.stopPropagation();
        closeMenu();
      });

      btn.addEventListener('click', function (e) {
        e.stopPropagation();
        if (menu.hidden && sub.hidden) {
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
          placeMenu();
          menu.hidden = true;
          sub.hidden = false;
          reportsBtn.setAttribute('aria-expanded', 'true');
          self.loadReportShortcuts();
        });
      }

      menu.addEventListener('click', function (e) {
        var item = e.target.closest('[data-bm]');
        if (!item || item === reportsBtn) return;
        e.stopPropagation();
        var action = item.getAttribute('data-bm');
        if (!MENU_KEEPS_OPEN[action]) closeMenu();
        self.menuAction(action);
      });

      sub.addEventListener('click', function (e) {
        var item = e.target.closest('[data-report]');
        if (!item) return;
        e.stopPropagation();
        var key = item.getAttribute('data-report');
        var label = item.textContent.trim();
        var link = item.getAttribute('data-bookmark-url');
        closeMenu();
        if (link) window.location.href = link;
        else self.reportAction(key, label);
      });

      document.addEventListener('click', function (e) {
        if ((!menu.hidden || !sub.hidden) &&
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
        case 'mass-update':
          MassUpdate.open();
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
          this.peopleDialog();
          break;
        case 'archive':
          window.location.href = '/api/boards/' + encodeURIComponent(boardId()) + '/archive';
          break;
        case 'recycle':
          this.openBin();
          break;
        case 'timeline':
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/timeline';
          break;
        case 'dark':
          this.toggleDark();
          break;
        case 'legend':
          this.toggleLegend();
          break;
        case 'large-names':
          this.toggleLargeNames();
          break;
        case 'help':
          this.openHelp();
          break;
      }
    },

    loadReportShortcuts: function () {
      var sub = document.getElementById('reports-submenu');
      var slugs = {
        burndown: 'burndown', flow: 'cumulative-flow', cycle: 'cycle-lead',
        throughput: 'throughput', montecarlo: 'monte-carlo',
      };
      ReportTools.send('GET', ReportTools.endpoint(boardId())).then(function (data) {
        Array.prototype.forEach.call(sub.querySelectorAll('.report-shortcut'), function (el) { el.remove(); });
        Object.keys(slugs).forEach(function (key) {
          var item = sub.querySelector('[data-report="' + key + '"]:not(.report-shortcut)');
          var after = item;
          (data.bookmarks || []).filter(function (b) {
            return b.shortcut && b.report === slugs[key];
          }).forEach(function (b) {
            var link = document.createElement('button');
            link.type = 'button';
            link.className = 'report-shortcut';
            link.setAttribute('role', 'menuitem');
            link.setAttribute('data-report', key);
            link.setAttribute('data-bookmark-url', ReportTools.bookmarkPath(boardId(), slugs[key], b.id));
            link.textContent = b.name;
            after.insertAdjacentElement('afterend', link);
            after = link;
          });
        });
      }).catch(function () {});
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
          PrintBoard.open(boardId());
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
          CalendarPage.open(boardId());
          break;
        case 'montecarlo':
          // KF-342: Monte Carlo forecasting report page — probabilistic completion forecast.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/monte-carlo';
          break;
        case 'estimate':
          // KF-345: Time estimate report page — estimated workload by color and column.
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/time-estimate';
          break;
        case 'column':
          window.location.href = '/b/' + encodeURIComponent(boardId()) + '/time-in-column';
          break;
        default:
          toast(label + ' report is not available in Pomb yet.');
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
      ['filter-reset', 'filter-reset-panel'].forEach(function (id) {
        var reset = document.getElementById(id);
        if (reset) reset.addEventListener('click', function (e) {
          e.preventDefault();
          self.resetFilter();
        });
      });
      var back = document.getElementById('filter-back');
      if (back) back.addEventListener('click', function () { self.closeFilter(); });
      // KF-215: the list button shows the label checkboxes; the field
      // lists the labels in use.
      var labelsToggle = document.getElementById('filter-labels-toggle');
      if (labelsToggle) labelsToggle.addEventListener('click', function () {
        var list = document.getElementById('filter-labels-list');
        list.hidden = !list.hidden;
        labelsToggle.setAttribute('aria-expanded', String(!list.hidden));
      });
      var labelsInput = document.getElementById('filter-labels-input');
      if (labelsInput) labelsInput.addEventListener('keydown', function (e) {
        if (e.key !== 'Enter') return;
        e.preventDefault();
        // The field lists the labels in use: Enter makes the checked set
        // the typed labels the board has, and clearing it removes the label
        // filter. Unknown labels are reported and ignored; a list with no
        // known label leaves the filter as it was.
        var typed = labelsInput.value.split(',').map(function (l) {
          return l.trim();
        }).filter(Boolean);
        var boxes = Array.prototype.slice.call(document.querySelectorAll('input[name="f-label"]'));
        var known = boxes.map(function (box) { return box.value.toLowerCase(); });
        var unknown = typed.filter(function (l) { return known.indexOf(l.toLowerCase()) === -1; });
        if (unknown.length) toast('Label not found: ' + unknown.join(', '));
        if (typed.length === 0 || unknown.length < typed.length) {
          var wanted = typed.map(function (l) { return l.toLowerCase(); });
          boxes.forEach(function (box) {
            box.checked = wanted.indexOf(box.value.toLowerCase()) !== -1;
          });
        }
        self.applyFilter();
        self.maybeSaveFilter();
      });
      panel.addEventListener('input', (e) => {
        if (e.target.id !== 'f-owner') return;
        self.applyFilter();
        self.maybeSaveFilter();
      });
      panel.addEventListener('change', function (e) {
        // KF-175: handle both select dropdowns (by id) and radio buttons (by name)
        if (e.target.id === 'f-user' || e.target.id === 'f-owner' || e.target.id === 'f-color' || e.target.id === 'f-date' ||
            e.target.id === 'f-label-mode' || e.target.id === 'f-hide-epics' || e.target.classList.contains('f-field') ||
            e.target.name === 'f-fold' || e.target.name === 'f-user' || e.target.name === 'f-color' || e.target.name === 'f-date' || e.target.name === 'f-label') {
          self.applyFilter();
          self.maybeSaveFilter();
        } else if (e.target.id === 'filter-remember') {
          self.maybeSaveFilter();
        } else if (e.target.id === 'f-epic') {
          self.filterOnEpic(e.target.value);
        }
      });
      this.initColumnLabels();
      this.restoreFilter();
      this.initFilterBookmarks();
      // KF-215: dock the panel below the top bars. Measure the dark topbar
      // + gray board-header row so the panel's own header (Filter title +
      // close button) renders below them instead of sliding underneath —
      // the panel can then never cover its own toggle, and the close
      // button stays reachable. Recompute on resize for font/zoom changes.
      this.positionFilterPanel();
      window.addEventListener('resize', function () { self.positionFilterPanel(); });
    },

    // KF-215: filter bookmarks in the panel footer, saved in this browser
    // per board.
    filterBookmarksKey: function () { return 'pomb-filter-bookmarks:' + boardId(); },
    readFilterBookmarks: function () {
      try { return JSON.parse(window.localStorage.getItem(this.filterBookmarksKey()) || '[]') || []; } catch (e) { return []; }
    },
    writeFilterBookmarks: function (list) {
      try { window.localStorage.setItem(this.filterBookmarksKey(), JSON.stringify(list)); } catch (e) { toast('Could not save the bookmark.'); }
      this.renderFilterBookmarks();
    },
    renderFilterBookmarks: function () {
      var self = this;
      var list = this.readFilterBookmarks();
      document.getElementById('filter-bookmarks-count').textContent = String(list.length);
      var ul = document.getElementById('filter-bookmarks-list');
      ul.innerHTML = '';
      list.forEach(function (b, i) {
        var li = document.createElement('li');
        var apply = document.createElement('button');
        apply.type = 'button';
        apply.className = 'filter-bookmark-apply';
        apply.textContent = b.name;
        apply.addEventListener('click', function () {
          self.setRadio('f-user', b.user);
          self.setOwner(b.owner);
          self.setRadio('f-color', b.color);
          self.setRadio('f-date', b.date);
          self.setLabelMode(b.labelMode);
          self.setFilterFields(b.fields);
          self.setHideEpics(b.hideEpics);
          document.querySelectorAll('input[name="f-label"]').forEach(function (box) {
            box.checked = (b.labels || []).indexOf(box.value) !== -1;
          });
          self.applyFilter();
          self.maybeSaveFilter();
        });
        var del = document.createElement('button');
        del.type = 'button';
        del.className = 'filter-bookmark-delete';
        del.setAttribute('aria-label', 'Delete bookmark ' + b.name);
        del.textContent = '×';
        del.addEventListener('click', function () {
          var next = self.readFilterBookmarks();
          next.splice(i, 1);
          self.writeFilterBookmarks(next);
        });
        li.appendChild(apply);
        li.appendChild(del);
        ul.appendChild(li);
      });
    },
    initFilterBookmarks: function () {
      var self = this;
      var toggle = document.getElementById('filter-bookmarks-toggle');
      if (!toggle) return;
      toggle.addEventListener('click', function () {
        var body = document.getElementById('filter-bookmarks-body');
        body.hidden = !body.hidden;
        toggle.setAttribute('aria-expanded', String(!body.hidden));
      });
      var name = document.getElementById('filter-bookmark-name');
      var save = function () {
        var label = name.value.trim();
        if (!label) { name.focus(); return; }
        var list = self.readFilterBookmarks();
        list.push({
          name: label,
          user: self.filterValue('f-user'),
          owner: self.ownerValue(),
          color: self.filterValue('f-color'),
          date: self.filterValue('f-date'),
          labels: self.filterLabels(),
          labelMode: self.filterLabelMode(),
          fields: self.filterFields(),
          hideEpics: self.hideEpics(),
        });
        name.value = '';
        self.writeFilterBookmarks(list);
      };
      document.getElementById('filter-bookmark-save').addEventListener('click', save);
      name.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') { e.preventDefault(); save(); }
      });
      this.renderFilterBookmarks();
    },

    // KF-215: KanbanFlow docks the panel directly under the top bar.
    positionFilterPanel: function () {
      var row = document.querySelector('.board-header-row');
      var topbar = document.querySelector('.topbar');
      var anchor = row || topbar;
      if (!anchor) return;
      var offset = Math.round(anchor.getBoundingClientRect().bottom);
      ['filter-panel', 'epics-panel'].forEach(function (id) {
        var panel = document.getElementById(id);
        if (!panel) return;
        panel.style.top = offset + 'px';
        panel.style.height = 'calc(100vh - ' + offset + 'px)';
      });
    },

    toggleFilter: function () {
      var panel = document.getElementById('filter-panel');
      if (!panel) return;
      if (panel.hidden) this.openFilter();
      else this.closeFilter();
    },
    openFilter: function () {
      var panel = document.getElementById('filter-panel');
      if (!panel) return false;
      this.closeEpics();
      this.positionFilterPanel();
      panel.hidden = false;
      this.syncFilterBtn();
      if (!this.epicsLoaded) this.loadEpics();
      return true;
    },
    initEpics: function () {
      var self = this;
      var btn = document.getElementById('epics-btn');
      var panel = document.getElementById('epics-panel');
      if (!btn || !panel) return;
      btn.addEventListener('click', function (e) {
        e.stopPropagation();
        self.toggleEpics();
      });
      ['epics-close', 'epics-back'].forEach(function (id) {
        var el = document.getElementById(id);
        if (el) el.addEventListener('click', function () { self.closeEpics(); });
      });
      panel.addEventListener('epics:open', function () { self.loadEpics(); });
      document.addEventListener('click', (e) => {
        const toggle = e.target.closest ? e.target.closest('.card-epic-toggle') : null;
        if (!toggle) return;
        const id = toggle.dataset.epicId;
        self.filterOnEpic(self.epicFilter === id ? '' : id);
      });
      this.syncEpicsBtn();
    },
    toggleEpics: function () {
      var panel = document.getElementById('epics-panel');
      if (!panel) return;
      if (panel.hidden) this.openEpics();
      else this.closeEpics();
    },
    openEpics: function () {
      var panel = document.getElementById('epics-panel');
      if (!panel) return false;
      this.closeFilter();
      this.positionFilterPanel();
      var wasHidden = panel.hidden;
      panel.hidden = false;
      this.syncEpicsBtn();
      if (wasHidden) panel.dispatchEvent(new CustomEvent('epics:open', { bubbles: true }));
      return true;
    },
    closeEpics: function () {
      var panel = document.getElementById('epics-panel');
      if (!panel) return;
      panel.hidden = true;
      this.syncEpicsBtn();
    },
    epics: [],
    epicsLoaded: false,
    epicFilter: '',
    epicsFolded: {},
    epicsClosedOpen: false,
    epicsTimer: null,
    epicsSeq: 0,
    epicsPanelOpen: function () {
      var panel = document.getElementById('epics-panel');
      return !!panel && !panel.hidden;
    },
    scheduleEpicsRefresh: function () {
      var self = this;
      if (!this.epicsPanelOpen()) return;
      if (this.epicsTimer) window.clearTimeout(this.epicsTimer);
      this.epicsTimer = window.setTimeout(function () {
        self.epicsTimer = null;
        self.loadEpics();
      }, EPICS_REFRESH_DEBOUNCE_MS);
    },
    loadEpics: function () {
      var self = this;
      var board = boardId();
      if (!board) return Promise.resolve();
      var seq = ++this.epicsSeq;
      return fetch('/api/boards/' + encodeURIComponent(board) + '/epics', {
        headers: { 'Accept': 'application/json' },
      })
        .then(function (res) {
          return res.ok ? res.json() : toastRefusal(res, 'Could not load epics.');
        })
        .then(function (list) {
          if (seq !== self.epicsSeq || !Array.isArray(list)) return;
          self.epics = list;
          self.epicsLoaded = true;
          self.fillEpicSelect();
          if (!self.epicsNewOpen) self.renderEpics();
        })
        .catch(function () { toast('Could not load epics.'); });
    },
    epicChoices: function (excludeId) {
      var self = this;
      var ready = this.epicsLoaded ? Promise.resolve() : this.loadEpics();
      return ready.then(function () {
        if (!self.epicsLoaded) return null;
        var barred = {};
        if (excludeId) barred[excludeId] = true;
        var grew = true;
        while (grew) {
          grew = false;
          self.epics.forEach(function (e) {
            if (e.parent_id && barred[e.parent_id] && !barred[e.id]) {
              barred[e.id] = true;
              grew = true;
            }
          });
        }
        var open = self.epics.filter(function (e) { return !e.closed && !barred[e.id]; });
        var openIds = {};
        open.forEach(function (e) { openIds[e.id] = true; });
        var byParent = {};
        var roots = [];
        open.forEach(function (e) {
          if (e.parent_id && openIds[e.parent_id] && e.parent_id !== e.id) {
            (byParent[e.parent_id] = byParent[e.parent_id] || []).push(e);
          } else {
            roots.push(e);
          }
        });
        var choices = [];
        var seen = {};
        function walk(epic, depth) {
          if (seen[epic.id]) return;
          seen[epic.id] = true;
          choices.push({ id: epic.id, name: epic.name, depth: depth });
          (byParent[epic.id] || []).forEach(function (child) { walk(child, depth + 1); });
        }
        roots.forEach(function (epic) { walk(epic, 0); });
        open.forEach(function (epic) { walk(epic, 0); });
        return choices;
      });
    },
    reopenEpic: function (id) {
      var self = this;
      return api('/api/tasks/' + encodeURIComponent(id) + '/reopen', 'POST')
        .then(function (res) {
          if (!res.ok) return toastRefusal(res, 'Could not reopen the epic.');
          return self.loadEpics();
        })
        .catch(function () { toast('Could not reopen the epic.'); });
    },
    epicDoneText: function (epic) {
      return (epic.children_done || 0) + ' of ' + (epic.children_total || 0) + ' done';
    },
    epicProgress: function (epic) {
      var total = epic.children_total || 0;
      var done = epic.children_done || 0;
      var bar = document.createElement('span');
      bar.className = 'tm-progress';
      bar.setAttribute('role', 'progressbar');
      bar.setAttribute('aria-label', 'Children done');
      bar.setAttribute('aria-valuemin', '0');
      bar.setAttribute('aria-valuemax', String(total));
      bar.setAttribute('aria-valuenow', String(done));
      var fill = document.createElement('span');
      fill.className = 'tm-progress-fill';
      fill.style.width = (total > 0 ? Math.min(100, (done * 100) / total) : 0) + '%';
      bar.appendChild(fill);
      var text = document.createElement('span');
      text.className = 'epic-done';
      text.textContent = this.epicDoneText(epic);
      var line = document.createElement('div');
      line.className = 'epic-progress';
      line.appendChild(bar);
      line.appendChild(text);
      return line;
    },
    epicNameButton: function (epic) {
      var name = document.createElement('button');
      name.type = 'button';
      name.className = 'epic-name';
      name.title = 'Open the epic';
      name.textContent = epic.name;
      name.addEventListener('click', function () { openModal(epic.id); });
      return name;
    },
    epicRow: function (epic, parent, kids) {
      var self = this;
      var lit = self.epicFilter === epic.id;
      var row = document.createElement('div');
      row.className = 'epic-row' + (lit ? ' is-lit' : '');
      row.dataset.epicId = epic.id;
      var head = document.createElement('div');
      head.className = 'epic-head';
      if (kids.length > 0) {
        var folded = !!self.epicsFolded[epic.id];
        var fold = document.createElement('button');
        fold.type = 'button';
        fold.className = 'epic-fold';
        fold.setAttribute('aria-expanded', folded ? 'false' : 'true');
        fold.setAttribute('aria-label', folded ? 'Show nested epics' : 'Hide nested epics');
        fold.textContent = folded ? '▸' : '▾';
        fold.addEventListener('click', function () {
          self.epicsFolded[epic.id] = !self.epicsFolded[epic.id];
          self.renderEpics();
        });
        head.appendChild(fold);
      }
      head.appendChild(self.epicNameButton(epic));
      head.appendChild(self.locateButton(epic));
      var funnel = document.createElement('button');
      funnel.type = 'button';
      funnel.className = 'board-bar-btn epic-funnel' + (lit ? ' is-lit' : '');
      funnel.setAttribute('aria-pressed', lit ? 'true' : 'false');
      funnel.dataset.epicId = epic.id;
      funnel.addEventListener('click', function () {
        self.filterOnEpic(self.epicFilter === epic.id ? '' : epic.id);
      });
      funnel.title = 'Filter the board to this epic';
      funnel.setAttribute('aria-label', 'Filter the board to this epic');
      funnel.innerHTML = '<svg width="12" height="12" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true"><path d="M2 3h12L9.5 8.5V13l-3 1.6V8.5L2 3z"/></svg>';
      head.appendChild(funnel);
      row.appendChild(head);
      if (parent) {
        var inside = document.createElement('div');
        inside.className = 'epic-inside';
        inside.textContent = 'inside ' + parent.name;
        row.appendChild(inside);
      }
      if (epic.column_name) {
        const where = document.createElement('div');
        where.className = 'epic-column';
        where.textContent = 'in ' + epic.column_name;
        row.appendChild(where);
      }
      row.appendChild(self.epicProgress(epic));
      return row;
    },
    LOCATE_MS: 2000,
    locateButton(epic) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'board-bar-btn epic-locate';
      button.dataset.epicId = epic.id;
      button.title = 'Show on board';
      button.setAttribute('aria-label', 'Show on board');
      button.innerHTML = '<svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="4.5" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="8" cy="8" r="1.3" fill="currentColor"/><path d="M8 1v3M8 12v3M1 8h3M12 8h3" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/></svg>';
      button.addEventListener('click', () => this.locateEpic(epic.id));
      return button;
    },
    locateEpic(id) {
      const card = [...document.querySelectorAll('.task-card')].find((el) => el.dataset.taskId === id);
      if (!card || card.hidden || card.style.display === 'none' || card.getClientRects().length === 0) {
        toast('Not shown on the board', 'Its card is hidden by a filter, a collapsed column or an archive page.');
        return;
      }
      card.scrollIntoView({ behavior: 'smooth', block: 'center', inline: 'center' });
      card.classList.remove('card-located');
      void card.offsetWidth;
      card.classList.add('card-located');
      window.setTimeout(() => card.classList.remove('card-located'), this.LOCATE_MS);
    },
    markEpicsSeen: function (epic, byParent, seen) {
      var self = this;
      if (seen[epic.id]) return;
      seen[epic.id] = true;
      (byParent[epic.id] || []).forEach(function (kid) { self.markEpicsSeen(kid, byParent, seen); });
    },
    epicBranch: function (epic, parent, byParent, seen) {
      var self = this;
      var kids = (byParent[epic.id] || []).filter(function (kid) { return !seen[kid.id]; });
      seen[epic.id] = true;
      var branch = document.createElement('div');
      branch.className = 'epic-branch';
      branch.appendChild(this.epicRow(epic, parent, kids));
      if (this.epicsFolded[epic.id]) {
        kids.forEach(function (kid) { self.markEpicsSeen(kid, byParent, seen); });
      } else if (kids.length > 0) {
        var nest = document.createElement('div');
        nest.className = 'epic-nest';
        for (var i = 0; i < kids.length; i++) {
          nest.appendChild(this.epicBranch(kids[i], epic, byParent, seen));
        }
        branch.appendChild(nest);
      }
      return branch;
    },
    canWrite: function () {
      return !document.querySelector('.board-wrap[data-role="viewer"]');
    },
    epicsNewOpen: false,
    epicsNewText: '',
    epicsNewBusy: false,
    closeNewEpic: function () {
      this.epicsNewOpen = false;
      this.epicsNewText = '';
      this.renderEpics();
    },
    createEpic: function () {
      var self = this;
      var name = this.epicsNewText.trim();
      var board = boardId();
      if (!name || !board || this.epicsNewBusy) return Promise.resolve();
      this.epicsNewBusy = true;
      this.renderEpics();
      return fetch('/api/tasks', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
        body: JSON.stringify({ kind: 'epic', board_id: board, name: name, description: document.getElementById('epics-panel').dataset.epicTemplate }),
      })
        .then(function (res) {
          return res.ok ? res.json() : toastRefusal(res, 'Could not create the epic.');
        })
        .then(function (task) {
          if (!task) return;
          self.epicsNewOpen = false;
          self.epicsNewText = '';
          self.renderEpics();
          return self.loadEpics().then(function () { openModal(task.id); });
        })
        .catch(function () { toast('Could not create the epic.'); })
        .then(function () {
          self.epicsNewBusy = false;
          self.renderEpics();
        });
    },
    newEpicSection: function () {
      var self = this;
      var section = document.createElement('section');
      section.className = 'filter-section';
      section.setAttribute('aria-label', 'New epic');
      if (!this.epicsNewOpen) {
        var open = document.createElement('button');
        open.type = 'button';
        open.className = 'btn btn-sm btn-block epic-new-btn';
        open.textContent = '+ New epic';
        open.addEventListener('click', function () {
          self.epicsNewOpen = true;
          self.renderEpics();
        });
        section.appendChild(open);
        return section;
      }
      var heading = document.createElement('h4');
      heading.textContent = 'New epic';
      var row = document.createElement('div');
      row.className = 'filter-bookmarks-add epic-new-row';
      var input = document.createElement('input');
      input.type = 'text';
      input.className = 'epic-new-input';
      input.maxLength = 500;
      input.value = this.epicsNewText;
      input.setAttribute('aria-label', 'Epic name');
      input.addEventListener('input', function () { self.epicsNewText = input.value; });
      input.disabled = this.epicsNewBusy;
      input.addEventListener('keydown', function (e) {
        e.stopPropagation();
        if (e.key === 'Enter') {
          e.preventDefault();
          self.epicsNewText = input.value;
          self.createEpic();
        } else if (e.key === 'Escape') {
          e.preventDefault();
          self.closeNewEpic();
        }
      });
      var create = document.createElement('button');
      create.type = 'button';
      create.className = 'btn btn-sm btn-primary epic-new-create';
      create.textContent = 'Create';
      create.disabled = this.epicsNewBusy;
      create.addEventListener('click', function () {
        self.epicsNewText = input.value;
        self.createEpic();
      });
      var cancel = document.createElement('button');
      cancel.type = 'button';
      cancel.className = 'btn btn-sm btn-ghost epic-new-cancel';
      cancel.textContent = 'Cancel';
      cancel.disabled = this.epicsNewBusy;
      cancel.addEventListener('click', function () { self.closeNewEpic(); });
      row.appendChild(input);
      row.appendChild(create);
      row.appendChild(cancel);
      section.appendChild(heading);
      section.appendChild(row);
      if (!this.epicsNewBusy) window.setTimeout(function () { input.focus(); }, 0);
      return section;
    },
    renderEpics: function () {
      var self = this;
      var body = document.getElementById('epics-body');
      if (!body) return;
      var open = this.epics.filter(function (e) { return !e.closed; });
      var closed = this.epics.filter(function (e) { return e.closed; });
      var openIds = {};
      open.forEach(function (e) { openIds[e.id] = true; });
      var byParent = {};
      var roots = [];
      open.forEach(function (e) {
        if (e.parent_id && openIds[e.parent_id] && e.parent_id !== e.id) {
          (byParent[e.parent_id] = byParent[e.parent_id] || []).push(e);
        } else {
          roots.push(e);
        }
      });
      body.textContent = '';
      if (this.canWrite()) body.appendChild(this.newEpicSection());
      var list = document.createElement('div');
      list.className = 'epics-list';
      var seen = {};
      function addGroup(epic) {
        if (seen[epic.id]) return;
        var group = document.createElement('div');
        group.className = 'epic-group';
        group.appendChild(self.epicBranch(epic, null, byParent, seen));
        list.appendChild(group);
      }
      roots.forEach(addGroup);
      open.forEach(addGroup);
      if (open.length === 0 && closed.length === 0) {
        var empty = document.createElement('p');
        empty.className = 'epics-empty';
        empty.textContent = 'No epics yet';
        list.appendChild(empty);
      }
      body.appendChild(list);
      if (closed.length > 0) body.appendChild(this.renderClosedEpics(closed));
    },
    renderClosedEpics: function (closed) {
      var self = this;
      var section = document.createElement('div');
      section.className = 'epics-closed';
      var toggle = document.createElement('button');
      toggle.type = 'button';
      toggle.className = 'filter-bookmarks-head epics-closed-toggle';
      toggle.setAttribute('aria-expanded', this.epicsClosedOpen ? 'true' : 'false');
      var label = document.createElement('span');
      label.textContent = (this.epicsClosedOpen ? '▾ ' : '▸ ') + closed.length + ' closed ' + (closed.length === 1 ? 'epic' : 'epics');
      toggle.appendChild(label);
      toggle.addEventListener('click', function () {
        self.epicsClosedOpen = !self.epicsClosedOpen;
        self.renderEpics();
      });
      section.appendChild(toggle);
      if (this.epicsClosedOpen) {
        var ul = document.createElement('ul');
        ul.className = 'filter-bookmarks-list epics-closed-list';
        closed.forEach(function (epic) {
          var li = document.createElement('li');
          li.dataset.epicId = epic.id;
          var info = document.createElement('span');
          info.className = 'epics-closed-info';
          info.appendChild(self.epicNameButton(epic));
          var done = document.createElement('span');
          done.className = 'epic-done';
          done.textContent = self.epicDoneText(epic);
          info.appendChild(done);
          li.appendChild(info);
          var reopen = document.createElement('button');
          reopen.type = 'button';
          reopen.className = 'filter-bookmark-apply epic-reopen';
          reopen.textContent = 'Reopen';
          reopen.addEventListener('click', function () {
            reopen.disabled = true;
            self.reopenEpic(epic.id).then(function () { reopen.disabled = false; });
          });
          if (self.canWrite()) li.appendChild(reopen);
          ul.appendChild(li);
        });
        section.appendChild(ul);
      }
      return section;
    },
    syncEpicsBtn: function () {
      var btn = document.getElementById('epics-btn');
      var panel = document.getElementById('epics-panel');
      if (!btn || !panel) return;
      btn.classList.toggle('active', !panel.hidden);
      btn.setAttribute('aria-expanded', panel.hidden ? 'false' : 'true');
    },
    showAllMembers: function () {
      var select = document.getElementById('f-user');
      if (!select) return false;
      select.value = 'all';
      select.dispatchEvent(new Event('change', { bubbles: true }));
      return true;
    },
    openReports: function () {
      var menu = document.getElementById('board-menu');
      var button = document.getElementById('board-menu-btn');
      var reports = menu ? menu.querySelector('[data-bm="reports"]') : null;
      var sub = document.getElementById('reports-submenu');
      if (!button || !reports || !sub) return false;
      if (menu.hidden && sub.hidden) button.click();
      reports.click();
      var first = sub.querySelector('[data-report]');
      if (first) first.focus();
      return true;
    },
    closeFilter: function () {
      var panel = document.getElementById('filter-panel');
      if (!panel) return;
      panel.hidden = true;
      this.syncFilterBtn();
    },
    syncLegendFilter: function () {
      var color = this.filterValue('f-color');
      document.querySelectorAll('.color-legend-segment').forEach(function (segment) {
        segment.classList.toggle('color-legend-segment--filtered',
          color !== 'all' && segment.dataset.colorValue === color);
      });
    },
    filterOnColor: function (value) {
      var select = document.getElementById('f-color');
      if (!select) return false;
      select.value = value;
      select.dispatchEvent(new Event('change', { bubbles: true }));
      return true;
    },
    renameLegendColor: function (segment) {
      var original = segment.textContent.trim();
      var input = document.createElement('input');
      input.type = 'text';
      input.className = 'color-legend-rename';
      input.maxLength = 50;
      input.value = original;
      segment.textContent = '';
      segment.appendChild(input);
      input.focus();
      input.select();
      var finished = false;
      function finish(save) {
        if (finished) return;
        finished = true;
        var label = input.value.trim();
        if (!save || !label || label === original) {
          segment.textContent = original;
          return;
        }
        api('/api/boards/' + encodeURIComponent(boardId()) + '/colors/' + encodeURIComponent(segment.dataset.colorId),
          'PATCH', { label: label }).then(function (res) {
          if (!res.ok) {
            segment.textContent = original;
            toast('Could not rename the color.');
            return;
          }
          segment.textContent = label;
          var swatch = document.querySelector('#board-colors span[data-id="' + cssEscape(segment.dataset.colorId) + '"]');
          if (swatch) swatch.dataset.label = label;
          var option = document.querySelector('#f-color option[value="' + cssEscape(segment.dataset.colorValue) + '"]');
          if (option) option.textContent = label;
        }).catch(function () {
          segment.textContent = original;
          toast('Could not rename the color.');
        });
      }
      input.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') { e.preventDefault(); finish(true); }
        else if (e.key === 'Escape') finish(false);
      });
      input.addEventListener('blur', function () { finish(true); });
    },
    initLegend: function () {
      var self = this;
      var menu = document.getElementById('legend-menu');
      var footer = document.querySelector('.color-legend');
      if (!menu || !footer) return;
      var target = null;
      footer.addEventListener('click', function (e) {
        var segment = e.target.closest('.color-legend-segment');
        if (!segment || e.target.closest('input')) return;
        e.stopPropagation();
        target = segment;
        var rect = segment.getBoundingClientRect();
        placeMenu(menu, rect.left, 0);
        menu.style.top = Math.max(4, rect.top - menu.offsetHeight - 4) + 'px';
      });
      menu.addEventListener('click', function (e) {
        var item = e.target.closest('[data-legend-act]');
        if (!item || !target) return;
        e.stopPropagation();
        hideFloatingMenus();
        if (item.getAttribute('data-legend-act') === 'rename') self.renameLegendColor(target);
        else self.filterOnColor(target.dataset.colorValue);
      });
    },
    fillEpicSelect: function () {
      var select = document.getElementById('f-epic');
      if (!select) return;
      var chosen = this.epicFilter;
      var known = {};
      this.epics.forEach(function (e) { known[e.id] = e; });
      select.textContent = '';
      var all = document.createElement('option');
      all.value = '';
      all.textContent = 'Show all';
      select.appendChild(all);
      this.epics.forEach(function (e) {
        if (e.closed && e.id !== chosen) return;
        var option = document.createElement('option');
        option.value = e.id;
        option.textContent = e.closed ? e.name + ' (closed)' : e.name;
        select.appendChild(option);
      });
      if (chosen && !known[chosen]) {
        var pending = document.createElement('option');
        pending.value = chosen;
        pending.textContent = this.epicsLoaded ? 'Epic not found' : 'Selected epic';
        select.appendChild(pending);
      }
      select.value = chosen;
    },
    hideEpics: function () {
      var box = document.getElementById('f-hide-epics');
      return !!box && box.checked;
    },
    setHideEpics: function (on) {
      var box = document.getElementById('f-hide-epics');
      if (box) box.checked = on === true;
    },
    setEpicFilter: function (id) {
      this.epicFilter = id || '';
      var select = document.getElementById('f-epic');
      if (select) select.value = this.epicFilter;
    },
    filterOnEpic: function (id) {
      this.setEpicFilter(id);
      this.applyFilter();
      this.maybeSaveFilter();
    },
    isEpicChild(card, epic) {
      const tag = card.querySelector('.card-epic-tag');
      return !!tag && tag.dataset.epicId === epic;
    },
    syncEpicFunnels: function () {
      var chosen = this.epicFilter;
      document.querySelectorAll('.task-card').forEach((card) => {
        card.classList.toggle('is-epic-child', !!chosen && this.isEpicChild(card, chosen));
      });
      document.querySelectorAll('.card-epic-toggle').forEach((toggle) => {
        const lit = !!chosen && toggle.dataset.epicId === chosen;
        toggle.setAttribute('aria-pressed', lit ? 'true' : 'false');
        const card = toggle.closest('.task-card');
        if (card) card.classList.toggle('is-lit', lit);
      });
      document.querySelectorAll('.epic-funnel').forEach(function (funnel) {
        var lit = !!chosen && funnel.dataset.epicId === chosen;
        funnel.classList.toggle('is-lit', lit);
        funnel.setAttribute('aria-pressed', lit ? 'true' : 'false');
        var row = funnel.closest('.epic-row');
        if (row) row.classList.toggle('is-lit', lit);
      });
    },
    syncFilterBtn: function () {
      this.syncLegendFilter();
      this.syncEpicFunnels();
      var btn = document.getElementById('filter-btn');
      var panel = document.getElementById('filter-panel');
      if (!btn || !panel) return;
      var filtering = this.isFiltering();
      btn.classList.toggle('active', !panel.hidden || filtering);
      btn.classList.toggle('filtering', filtering);
      var reset = document.getElementById('filter-reset');
      if (reset) reset.hidden = !filtering || !panel.hidden;
      var panelReset = document.getElementById('filter-reset-panel');
      if (panelReset) panelReset.hidden = !filtering || panel.hidden;
      var labelsInput = document.getElementById('filter-labels-input');
      if (labelsInput) labelsInput.value = this.filterLabels().join(', ');
    },
    columnLabels: {},
    initColumnLabels: function () {
      var self = this;
      document.querySelectorAll('.columnHeader-marks').forEach(function (marks) {
        var th = marks.closest('.columnHeader');
        var colId = th.dataset.columnId;
        var clear = marks.querySelector('.columnHeader-markClear');
        function show(label) {
          if (label) self.columnLabels[colId] = label; else delete self.columnLabels[colId];
          marks.querySelectorAll('.columnHeader-mark').forEach(function (btn) {
            btn.classList.toggle('is-active', btn.dataset.markLabel === label);
            btn.setAttribute('aria-pressed', btn.dataset.markLabel === label ? 'true' : 'false');
          });
          clear.hidden = !label;
          clear.querySelector('.columnHeader-markClearLabel').textContent = label || '';
          clear.setAttribute('aria-label', label ? 'Clear the ' + label + ' filter of this column' : '');
          self.applyFilter();
        }
        marks.addEventListener('click', function (e) {
          var btn = e.target.closest('.columnHeader-mark');
          if (btn) {
            show(self.columnLabels[colId] === btn.dataset.markLabel ? null : btn.dataset.markLabel);
          } else if (e.target.closest('.columnHeader-markClear')) {
            show(null);
          }
        });
      });
    },
    refreshMarks: function () {
      this.scheduleEpicsRefresh();
      document.querySelectorAll('.columnHeader-marks').forEach(function (marks) {
        var colId = marks.closest('.columnHeader').dataset.columnId;
        var cards = document.querySelectorAll('.task-list[data-column-id="' + cssEscape(colId) + '"] .task-card');
        var clear = marks.querySelector('.columnHeader-markClear');
        var any = !clear.hidden;
        marks.querySelectorAll('.columnHeader-mark').forEach(function (btn) {
          var n = 0;
          cards.forEach(function (card) {
            var held = [];
            try { held = JSON.parse(card.dataset.labels || '[]'); } catch (e) { held = []; }
            if (held.indexOf(btn.dataset.markLabel) !== -1) n++;
          });
          btn.querySelector('.columnHeader-markCount').textContent = String(n);
          btn.hidden = n === 0;
          if (n > 0) any = true;
        });
        marks.hidden = !any;
      });
    },
    columnLabelFiltering: function () {
      return Object.keys(this.columnLabels).length > 0;
    },
    isFiltering: function () {
      return this.columnLabelFiltering() || this.filterValue('f-user') !== 'all' ||
        this.ownerValue() !== 'all' ||
        this.filterValue('f-color') !== 'all' ||
        this.filterValue('f-date') !== 'all' ||
        this.epicFilter !== '' ||
        this.hideEpics() ||
        this.filterLabels().length > 0 ||
        Object.keys(this.filterFields()).length > 0;
    },
    filterLabelMode: function () {
      var select = document.getElementById('f-label-mode');
      return select && select.value === this.LABEL_MODE.OR ? this.LABEL_MODE.OR : this.LABEL_MODE.AND;
    },
    setLabelMode: function (mode) {
      var select = document.getElementById('f-label-mode');
      if (select) select.value = mode === this.LABEL_MODE.OR ? this.LABEL_MODE.OR : this.LABEL_MODE.AND;
    },
    filterFields: function () {
      var chosen = {};
      document.querySelectorAll('.f-field').forEach(function (select) {
        if (select.value !== '') chosen[select.dataset.fieldKey] = select.value;
      });
      return chosen;
    },
    setFilterFields: function (chosen) {
      chosen = chosen || {};
      document.querySelectorAll('.f-field').forEach(function (select) {
        var value = chosen[select.dataset.fieldKey];
        var known = Array.prototype.some.call(select.options, function (option) {
          return option.value === value;
        });
        select.value = known ? value : '';
      });
    },
    resetFilter: function () {
      this.setRadio('f-user', 'all');
      this.setOwner('all');
      this.setRadio('f-color', 'all');
      this.setRadio('f-date', 'all');
      this.setLabelMode(this.LABEL_MODE.AND);
      this.setFilterFields({});
      this.setEpicFilter('');
      this.setHideEpics(false);
      this.setRadio('f-fold', this.FOLD_MODE.HIDE);
      document.querySelectorAll('input[name="f-label"]').forEach(function (box) {
        box.checked = false;
      });
      this.applyFilter();
      this.maybeSaveFilter();
    },
    filterValue: function (name) {
      // KF-175: support both select dropdowns (new) and radio buttons (legacy)
      var select = document.getElementById(name);
      if (select && select.tagName === 'SELECT') return select.value;
      var el = document.querySelector('input[name="' + name + '"]:checked');
      return el ? el.value : 'all';
    },
    ownerChoice: (match) => [...document.querySelectorAll('#f-owner-list option')].find(match),
    ownerValue: function () {
      const input = document.getElementById('f-owner');
      const typed = input ? input.value.trim().toLowerCase() : '';
      if (!typed) return 'all';
      const found = this.ownerChoice((option) => option.value.toLowerCase() === typed);
      return found ? found.dataset.owner : 'all';
    },
    setOwner: function (key) {
      const input = document.getElementById('f-owner');
      if (!input) return;
      const found = this.ownerChoice((option) => option.dataset.owner === key);
      input.value = found ? found.value : '';
    },
    setRadio: function (name, value) {
      if (!value) return;
      // KF-175: support select dropdowns
      var select = document.getElementById(name);
      if (select && select.tagName === 'SELECT') {
        // A saved "me" means the signed-in user; a value with no
        // option leaves the select as it is.
        if (name === 'f-user' && (value === 'me' || value === 'mine')) {
          value = Array.prototype.reduce.call(select.options, function (found, option) {
            return option.textContent.trim() === select.dataset.me ? option.value : found;
          }, value);
        }
        var known = Array.prototype.some.call(select.options, function (option) {
          return option.value === value;
        });
        if (known) select.value = value;
        return;
      }
      var el = document.querySelector('input[name="' + name + '"][value="' + value + '"]');
      if (el) el.checked = true;
    },

    scrollMemory: new WeakMap(),
    cardShown(card) {
      return !card.hidden && card.style.display !== 'none';
    },
    scrollAnchor(list) {
      if (typeof list.scrollTop !== 'number' || !list.getBoundingClientRect) return null;
      const top = list.getBoundingClientRect().top;
      const cards = [...list.querySelectorAll('.task-card, .filter-fold')];
      const at = cards.findIndex((card) => this.cardShown(card) && card.getBoundingClientRect().bottom > top);
      if (at === -1) return null;
      return { card: cards[at], offset: cards[at].getBoundingClientRect().top - top, earlier: cards.slice(0, at) };
    },
    scrollToAnchor(list, anchor) {
      if (!anchor) return;
      const successor = anchor.card._run ? anchor.card._run.slice(0, 1) : [];
      const kept = [anchor.card, ...successor, ...anchor.earlier.slice().reverse()]
        .find((card) => list.contains(card) && this.cardShown(card));
      if (!kept) return;
      const offset = kept === anchor.card || kept === successor[0] ? anchor.offset : 0;
      list.scrollTop += kept.getBoundingClientRect().top - list.getBoundingClientRect().top - offset;
    },
    holdScroll(list, change) {
      const anchor = list ? this.scrollAnchor(list) : null;
      change();
      if (!anchor) return;
      this.scrollToAnchor(list, anchor);
      const memory = this.scrollMemory.get(list);
      if (memory) memory.shown = list.scrollTop;
    },
    noteScroll() {
      return [...document.querySelectorAll('.task-list')].map((list) => {
        const anchor = this.scrollAnchor(list);
        const memory = this.scrollMemory.get(list);
        if (memory && anchor && list.scrollTop !== memory.shown) memory.moved = anchor;
        return { list, anchor };
      });
    },
    keepScroll(places) {
      const filtering = this.isFiltering();
      places.forEach(({ list, anchor }) => {
        let memory = this.scrollMemory.get(list);
        let target = anchor;
        if (!filtering) {
          if (memory) target = memory.moved || memory.base;
          this.scrollMemory.delete(list);
        } else if (!memory) {
          memory = { base: anchor, moved: null, shown: 0 };
          this.scrollMemory.set(list, memory);
        }
        this.scrollToAnchor(list, target);
        if (memory && filtering) memory.shown = list.scrollTop;
      });
    },

    applyFilter: function () {
      const places = this.noteScroll();
      var user = this.filterValue('f-user');
      const owner = this.ownerValue();
      var color = this.filterValue('f-color');
      var date = this.filterValue('f-date');
      var labels = this.filterLabels();
      var anyLabel = this.filterLabelMode() === this.LABEL_MODE.OR;
      var fields = this.filterFields();
      var fieldKeys = Object.keys(fields);
      var epic = this.epicFilter;
      var hideEpics = this.hideEpics();
      var self = this;
      var activeTaskId = null;
      if (user === 'timer' && window.TimerUI && TimerUI.state && TimerUI.state.taskId) {
        activeTaskId = String(TimerUI.state.taskId);
      }
      document.querySelectorAll('.task-card').forEach(function (card) {
        var show = true;
        if (color !== 'all' && card.dataset.colorValue !== color) show = false;
        // "Timer users": only the card with the running timer stays visible.
        if (user === 'timer' && String(card.dataset.taskId) !== activeTaskId) show = false;
        // KF-370: Unassigned and named users match the card's members.
        var members = (card.dataset.memberIds || '').split(',').filter(Boolean);
        if (user === 'unassigned' && members.length) show = false;
        if (user.indexOf('user:') === 0 && members.indexOf(user.slice(5)) === -1) show = false;
        const cardOwner = card.dataset.owner || '';
        if (owner === 'none' ? cardOwner !== '' : owner !== 'all' && cardOwner !== owner) show = false;
        // KF-159: date filters evaluate the card's data-due-at (RFC3339).
        if (!self.dateMatches(date, card.dataset.dueAt, card.dataset.dueDone === '1')) show = false;
        // KF-165: label filters match the card's data-labels JSON array.
        // Several labels match every one, or any one in "or" mode.
        if (show && labels.length) {
          var cardLabels = [];
          try { cardLabels = JSON.parse(card.dataset.labels || '[]'); } catch (e) { cardLabels = []; }
          var hasLabel = function (l) { return cardLabels.indexOf(l) !== -1; };
          if (!(anyLabel ? labels.some(hasLabel) : labels.every(hasLabel))) show = false;
        }
        if (show && fieldKeys.length) {
          var cardFields = {};
          try { cardFields = JSON.parse(card.dataset.filterFields || '{}'); } catch (e) { cardFields = {}; }
          if (!fieldKeys.every(function (k) { return cardFields[k] === fields[k]; })) show = false;
        }
        if (hideEpics && card.dataset.kind === CARD_KIND.EPIC) show = false;
        if (show && epic && card.dataset.taskId !== epic && !self.isEpicChild(card, epic)) show = false;
        var list = card.closest('.task-list');
        var only = list ? self.columnLabels[list.dataset.columnId] : null;
        if (show && only) {
          var held = [];
          try { held = JSON.parse(card.dataset.labels || '[]'); } catch (e) { held = []; }
          if (held.indexOf(only) === -1) show = false;
        }
        card.style.display = show ? '' : 'none';
      });
      this.pageArchives();
      this.refold();
      this.refreshFilteredCounts();
      this.syncFilterBtn();
      this.keepScroll(places);
    },
    ARCHIVE_PAGE_SIZE: 20,
    pageArchives() {
      document.querySelectorAll('[data-load-more]').forEach((more) => {
        const list = more.closest('.task-list');
        if (list) this.pageArchive(list, false);
      });
    },
    pageArchive(list, loadMore) {
      this.removeFolds();
      const cards = [...list.querySelectorAll('.task-card')];
      const filtering = this.isFiltering();
      const matches = (card) => !filtering || card.style.display !== 'none';
      let from = 0;
      if (loadMore) {
        from = cards.findIndex((card) => card.hidden);
        if (from === -1) from = cards.length;
      } else {
        cards.forEach((card) => { card.hidden = true; });
      }
      let revealed = 0;
      for (let i = from; i < cards.length && revealed < this.ARCHIVE_PAGE_SIZE; i++) {
        cards[i].hidden = false;
        if (matches(cards[i])) revealed++;
      }
      list.querySelectorAll('.done-group-label').forEach((label) => {
        let el = label.nextElementSibling;
        let shown = false;
        while (el && el.classList.contains('task-card')) {
          if (!el.hidden && matches(el)) { shown = true; break; }
          el = el.nextElementSibling;
        }
        label.hidden = !shown;
      });
      const left = cards.some((card) => card.hidden && matches(card));
      const button = list.querySelector('[data-load-more]');
      if (button) button.hidden = !left;
    },
    foldMode: function () {
      return this.epicFilter ? this.FOLD_MODE.COLLAPSE : this.panelFoldMode();
    },
    panelFoldMode() {
      return this.filterValue('f-fold') === this.FOLD_MODE.COLLAPSE ? this.FOLD_MODE.COLLAPSE : this.FOLD_MODE.HIDE;
    },
    removeFolds: function () {
      document.querySelectorAll('.filter-fold').forEach(function (row) { row.remove(); });
    },
    unfold: function () {
      this.cancelOpen();
      this.cancelClose();
      this.closeFold();
      this.removeFolds();
    },
    refold: function () {
      this.cancelOpen();
      this.cancelClose();
      this.openedFold = null;
      this.removeFolds();
      if (this.foldMode() !== this.FOLD_MODE.COLLAPSE || !this.isFiltering()) return;
      document.querySelectorAll('.task-list').forEach(function (list) {
        var run = [];
        var flush = function () {
          if (!run.length) return;
          var row = document.createElement('div');
          row.className = 'filter-fold';
          row.textContent = run.length + ' other card' + (run.length === 1 ? '' : 's');
          row._run = run;
          row._list = list;
          list.insertBefore(row, run[0]);
          run = [];
        };
        Array.prototype.slice.call(list.children).forEach(function (el) {
          if (!el.classList.contains('task-card')) { flush(); return; }
          if (el.hidden) return;
          if (el.style.display === 'none') run.push(el);
          else flush();
        });
        flush();
      });
    },
    openFold: function (row) {
      this.cancelClose();
      if (this.openedFold === row) return;
      this.closeFold();
      this.holdScroll(row._list, () => {
        row._run.forEach((card) => { card.style.display = ''; });
        row.style.display = 'none';
      });
      this.openedFold = row;
    },
    closeFold: function () {
      var row = this.openedFold;
      if (!row) return;
      this.holdScroll(row._list, () => {
        row._run.forEach((card) => { card.style.display = 'none'; });
        row.style.display = '';
      });
      this.openedFold = null;
    },
    foldDragOver: function (target) {
      if (!target || !target.closest) return;
      var row = target.closest('.filter-fold');
      if (row) {
        if (this.openedFold && this.openedFold !== row) this.scheduleClose();
        this.scheduleOpen(row);
        return;
      }
      this.cancelOpen();
      var open = this.openedFold;
      if (!open) return;
      var list = target.closest('.task-list');
      if (list && list === open._list) {
        var card = target.closest('.task-card');
        if (!card || open._run.indexOf(card) !== -1 || card.classList.contains('sortable-placeholder')) {
          this.cancelClose();
          return;
        }
      }
      this.scheduleClose();
    },
    scheduleOpen: function (row) {
      if (this.pendingFold === row) return;
      this.cancelOpen();
      this.pendingFold = row;
      this.openTimer = window.setTimeout(() => {
        this.openTimer = null;
        this.pendingFold = null;
        this.openFold(row);
      }, this.OPEN_DWELL_MS);
    },
    scheduleClose: function () {
      if (this.closeTimer) return;
      this.closeTimer = window.setTimeout(() => {
        this.closeTimer = null;
        this.closeFold();
      }, this.CLOSE_GRACE_MS);
    },
    cancelOpen: function () {
      if (this.openTimer) window.clearTimeout(this.openTimer);
      this.openTimer = null;
      this.pendingFold = null;
    },
    cancelClose: function () {
      if (this.closeTimer) window.clearTimeout(this.closeTimer);
      this.closeTimer = null;
    },
    // KF-161: column header counts reflect the currently filtered (visible)
    // card set, KanbanFlow parity. When no filter is active the
    // server-rendered totals are restored from th.dataset.taskCount.
    refreshFilteredCounts: function () {
      var filtering = this.isFiltering();
      document.querySelectorAll('.columnHeader[data-column-id]').forEach(function (th) {
        var colId = th.dataset.columnId;
        var count;
        var minutes;
        var overdue;
        if (filtering) {
          count = 0;
          minutes = 0;
          overdue = 0;
          var sel = '.task-list[data-column-id="' + cssEscape(colId) + '"] .task-card';
          document.querySelectorAll(sel).forEach(function (card) {
            if (card.style.display === 'none' || card.dataset.kind === CARD_KIND.EPIC) return;
            count++;
            minutes += cardEstimateMinutes(card);
            if (card.dataset.overdue === '1') overdue++;
          });
        } else {
          count = parseInt(th.dataset.taskCount, 10) || 0;
          minutes = parseInt(th.dataset.estimateMinutes, 10) || 0;
          overdue = parseInt(th.dataset.overdueCount, 10) || 0;
        }
        var limit = wipLimitState(th, count, minutes);
        var el = th.querySelector('.columnHeader-count');
        if (el) el.textContent = filtering ? String(count) : limit.label;
        // KF-297/KF-371: the badge follows the visible set too.
        setSumBadge(th, count, minutes);
        // Mirror updateColumnCount's WIP-violation warning for the visible set.
        var warn = limit.warn;
        th.classList.toggle('columnHeader--warning', warn);
        th.classList.toggle('columnHeader--limitWarning', warn);
        syncWarningLine(th, warn, overdue);
        // KF-440: the collapsed strip's count colour, the overdue strip and
        // the strip's tooltip follow the visible set too.
        var badge = th.querySelector('.columnHeader-countBadge');
        if (badge && th.dataset.collapsed === '1') badge.classList.toggle('columnHeader-countBadge--over', warn);
        var overdueTitle = 'Overdue tasks: ' + overdue + ' / Total tasks: ' + count;
        if (th.dataset.collapsed === '1') {
          th.title = (overdue > 0 ? overdueTitle : 'Task count: ' + count) + ' / Click to expand';
        }
        document.querySelectorAll('.board-cell[data-column-id="' + cssEscape(colId) + '"] .columnOverdue').forEach(function (strip) {
          strip.hidden = overdue === 0;
          strip.title = overdueTitle;
          var text = strip.querySelector('.columnOverdue-text');
          if (text) text.textContent = overdue + ' overdue task' + (overdue === 1 ? '' : 's');
        });
      });
    },
    // KF-159: due-date matching for the filter panel. Undated tasks are
    // hidden for every specific range; only "Show all" keeps them and
    // "No due date" shows only them (KanbanFlow parity).
    dateMatches: function (value, dueAt, dueDone) {
      if (value === 'all') return true;
      if (!dueAt) return value === 'none';
      if (value === 'none') return false;
      var d = parseDueAt(dueAt);
      if (isNaN(d.getTime())) return false;
      var day = new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
      var now = new Date();
      var today = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
      var DAY = 86400000;
      // KF-219: "Overdue" compares the full due timestamp, not the date —
      // a task due later today is not overdue (KanbanFlow parity).
      if (value === 'overdue') return !dueDone && d.getTime() < Date.now();
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
        var restoring = self._pendingLabelFilter && self._pendingLabelFilter.length;
        if ((!labels || !labels.length) && !restoring) {
          wrap.innerHTML = '<span class="empty-note">No labels on this board yet.</span>';
          return;
        }
        (labels || []).forEach(function (l) { self.addLabelOption(wrap, l); });
        // A restored label filter applies once its options exist.
        self.restoreLabelFilter();
      }).catch(function () {
        wrap.innerHTML = '<span class="empty-note">Could not load labels.</span>';
      });
    },
    addLabelOption: function (wrap, l) {
      var label = document.createElement('label');
      label.className = 'filter-opt';
      var input = document.createElement('input');
      input.type = 'checkbox';
      input.name = 'f-label';
      input.value = l;
      label.appendChild(input);
      label.appendChild(document.createTextNode(' ' + l));
      wrap.appendChild(label);
      return input;
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
      var self = this;
      var boxes = wrap.querySelectorAll('input[name="f-label"]');
      var have = [];
      for (var i = 0; i < boxes.length; i++) {
        have.push(boxes[i].value);
        if (pending.indexOf(boxes[i].value) !== -1) boxes[i].checked = true;
      }
      pending.forEach(function (l) {
        if (have.indexOf(l) === -1) self.addLabelOption(wrap, l).checked = true;
      });
      this.applyFilter();
    },

    sessionFilterKey: function () { return 'pomb-filter-session:' + boardId(); },
    maybeSaveFilter: function () {
      var remember = document.getElementById('filter-remember');
      var state = JSON.stringify({
        user: this.filterValue('f-user'),
        owner: this.ownerValue(),
        color: this.filterValue('f-color'),
        date: this.filterValue('f-date'),
        labels: this.filterLabels(),
        labelMode: this.filterLabelMode(),
        fields: this.filterFields(),
        epic: this.epicFilter,
        hideEpics: this.hideEpics(),
        fold: this.panelFoldMode(),
      });
      try { window.sessionStorage.setItem(this.sessionFilterKey(), state); } catch (e) {}
      try {
        if (remember && remember.checked) {
          window.localStorage.setItem('pomb-filter', state);
        } else {
          window.localStorage.removeItem('pomb-filter');
        }
      } catch (e) { /* storage unavailable */ }
    },
    restoreFilter: function () {
      var raw = null;
      var kept = null;
      try { raw = window.localStorage.getItem('pomb-filter'); } catch (e) { /* ignore */ }
      try { kept = window.sessionStorage.getItem(this.sessionFilterKey()); } catch (e) {}
      if (!raw && !kept) return;
      try {
        var f = JSON.parse(kept || raw);
        this.setRadio('f-user', f.user);
        this.setOwner(f.owner);
        this.setRadio('f-color', f.color);
        this.setRadio('f-date', f.date);
        this.setLabelMode(f.labelMode);
        this.setFilterFields(f.fields);
        this.setEpicFilter(kept && typeof f.epic === 'string' ? f.epic : '');
        this.setHideEpics(f.hideEpics);
        this.setRadio('f-fold', f.fold === this.FOLD_MODE.COLLAPSE ? this.FOLD_MODE.COLLAPSE : this.FOLD_MODE.HIDE);
        this.fillEpicSelect();
        // Label checkboxes load async — apply them when they arrive.
        this._pendingLabelFilter = (f.labels && f.labels.length) ? f.labels : null;
        var remember = document.getElementById('filter-remember');
        if (remember && raw) remember.checked = true;
        this.applyFilter();
      } catch (e) { /* corrupt saved filter */ }
    },

    // Month/year date options depend on the current date — render in JS.
    // KF-381: appended to the Date select (KF-175), after "Due in 90 days".
    addDateOptions: function () {
      var select = document.getElementById('f-date');
      if (!select || select.dataset.extended) return;
      select.dataset.extended = '1';
      var now = new Date();
      var months = ['January', 'February', 'March', 'April', 'May', 'June',
        'July', 'August', 'September', 'October', 'November', 'December'];
      var nm = new Date(now.getFullYear(), now.getMonth() + 1, 1);
      var specs = [
        ['month:' + now.getFullYear() + '-' + now.getMonth(), 'Due in ' + months[now.getMonth()]],
        ['month:' + nm.getFullYear() + '-' + nm.getMonth(), 'Due in ' + months[nm.getMonth()]],
        ['year:' + now.getFullYear(), 'Due in ' + now.getFullYear()],
        ['year:' + (now.getFullYear() + 1), 'Due in ' + (now.getFullYear() + 1)],
      ];
      specs.forEach(function (s) {
        var option = document.createElement('option');
        option.value = s[0];
        option.textContent = s[1];
        select.appendChild(option);
      });
    },

    initMemberPopover: function () {
      var self = this;
      var popover = document.getElementById('member-popover');
      if (!popover) return;
      var labels = { owner: 'Owner', member: 'Member', viewer: 'Viewer' };
      var current = null;
      var close = function () {
        popover.hidden = true;
        current = null;
      };
      document.querySelectorAll('.board-member-avatar').forEach(function (avatar) {
        avatar.addEventListener('click', function (e) {
          e.stopPropagation();
          if (current === avatar) { close(); return; }
          current = avatar;
          document.getElementById('member-popover-name').textContent = avatar.dataset.memberName;
          document.getElementById('member-popover-role').textContent = labels[avatar.dataset.memberRole];
          popover.dataset.memberId = avatar.dataset.memberId;
          popover.hidden = false;
          popover.style.left = avatar.offsetLeft + 'px';
        });
      });
      document.getElementById('member-popover-filter').addEventListener('click', function () {
        self.setRadio('f-user', 'user:' + popover.dataset.memberId);
        self.applyFilter();
        self.maybeSaveFilter();
        self.openFilter();
        close();
      });
      document.addEventListener('click', function (e) {
        if (!popover.hidden && !e.target.closest('#member-popover')) close();
      });
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape') close();
      });
    },

    // ----- board-bar buttons -----
    initBarButtons: function () {
      var self = this;
      this.initMemberPopover();
      var invite = document.getElementById('invite-btn');
      if (invite) invite.addEventListener('click', function () {
        self.peopleDialog();
      });
      var layout = document.getElementById('edit-layout-btn');
      if (layout) layout.addEventListener('click', function () { self.editLayout(); });
      var layoutClose = document.getElementById('layout-view-close');
      if (layoutClose) layoutClose.addEventListener('click', function () { self.exitLayout(); });
      var layoutBack = document.getElementById('layout-back');
      if (layoutBack) layoutBack.addEventListener('click', function () { self.exitLayout(); });
      var layoutAddCol = document.getElementById('layout-add-column');
      if (layoutAddCol) layoutAddCol.addEventListener('click', function () {
        openAddColumnDialog('end', layoutAddCol);
      });
      // KF-246: green "Add column" button at the right end of the column list;
      // same add-column flow as the header button.
      // KF-405: column deletion lives in the layout view's column list.
      var layoutCols = document.getElementById('layout-columns-list');
      if (layoutCols) layoutCols.addEventListener('click', function (e) {
        var kebab = e.target.closest('[data-layout-column-menu]');
        if (!kebab) return;
        e.stopPropagation();
        layoutMenuColumnId = kebab.getAttribute('data-layout-column-menu');
        var items = Array.prototype.slice.call(layoutCols.querySelectorAll('[data-column-id]'));
        var at = items.findIndex(function (li) { return li.dataset.columnId === layoutMenuColumnId; });
        var menu = document.getElementById('layout-column-menu');
        menu.querySelector('[data-layout-col-act="left"]').hidden = at <= 0;
        menu.querySelector('[data-layout-col-act="right"]').hidden = at < 0 || at >= items.length - 1;
        var box = kebab.getBoundingClientRect();
        placeMenu(menu, box.left, box.bottom + 4);
      });
      var layoutColMenu = document.getElementById('layout-column-menu');
      if (layoutColMenu) layoutColMenu.addEventListener('click', function (e) {
        var btn = e.target.closest('[data-layout-col-act]');
        if (!btn || !layoutMenuColumnId) return;
        var id = layoutMenuColumnId;
        var act = btn.getAttribute('data-layout-col-act');
        hideFloatingMenus();
        if (act === 'edit') openEditColumnDialog(id);
        else if (act === 'left') moveColumnBy(id, -1);
        else if (act === 'right') moveColumnBy(id, 1);
        else if (act === 'add-left') openAddColumnDialog({ anchor: id, side: 'left' }, null);
        else if (act === 'add-right') openAddColumnDialog({ anchor: id, side: 'right' }, null);
        else if (act === 'delete') deleteColumn(id);
      });
      var layoutLanes = document.getElementById('layout-swimlanes-list');
      if (layoutLanes) layoutLanes.addEventListener('click', function (e) {
        var kebab = e.target.closest('[data-layout-lane-menu]');
        if (!kebab) return;
        e.stopPropagation();
        var box = kebab.getBoundingClientRect();
        openLaneMenu(kebab.getAttribute('data-layout-lane-menu'), 'layout', box.left, box.bottom + 4);
      });
      var layoutMerge = document.getElementById('layout-merge-swimlanes');
      if (layoutMerge) layoutMerge.addEventListener('click', confirmMergeAll);
      var layoutAddColEnd = document.getElementById('layout-add-column-end');
      if (layoutAddColEnd) layoutAddColEnd.addEventListener('click', function () {
        openAddColumnDialog('end', layoutAddColEnd);
      });
      var layoutAddSwim = document.getElementById('layout-add-swimlane');
      if (layoutAddSwim) layoutAddSwim.addEventListener('click', function () {
        addSwimlane(layoutAddSwim);
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

      window.history.replaceState(null, '', '#layout');
    },

    exitLayout: function () {
      var board = document.querySelector('main.board-wrap');
      var view = document.getElementById('layout-view');
      if (board) board.hidden = false;
      if (view) view.hidden = true;
      window.history.replaceState(null, '', window.location.pathname + window.location.search);
      // renderPill restores the pill's own visibility state.
      if (typeof TimerUI !== 'undefined' && TimerUI.renderPill) TimerUI.renderPill();
    },

    peopleDialog: function () {
      var dlg = document.getElementById('people-dialog');
      if (!dlg) return;
      var wrap = document.querySelector('.board-wrap');
      var roleLabels = { owner: 'Owner', member: 'Member', viewer: 'Viewer' };
      var assignable = ['member', 'viewer'];
      var isOwner = wrap.dataset.role === 'owner';
      var account = wrap.dataset.account;
      var base = '/api/boards/' + encodeURIComponent(boardId()) + '/members';
      var list = document.getElementById('people-list');
      var form = document.getElementById('people-add');
      var leave = document.getElementById('people-leave');
      var error = document.getElementById('people-error');
      form.hidden = !isOwner;
      leave.hidden = isOwner;

      function fail(message) {
        error.textContent = message;
        error.hidden = false;
      }

      function respond(res) {
        if (res.ok) {
          error.hidden = true;
          return reload();
        }
        var shown = res.status === 400 || res.status === 404;
        return res.text().then(function (text) {
          fail(shown && text ? text : 'Could not change the members.');
          return reload();
        });
      }

      function send(request) {
        return request.then(respond).catch(function () { fail('Network error.'); });
      }

      function roleCell(member) {
        if (member.role === 'owner' || !isOwner) {
          var label = document.createElement('span');
          label.className = 'people-role';
          label.textContent = roleLabels[member.role];
          return label;
        }
        var select = document.createElement('select');
        select.className = 'people-role-select';
        select.setAttribute('aria-label', 'Role of ' + member.username);
        assignable.forEach(function (role) {
          var option = document.createElement('option');
          option.value = role;
          option.textContent = roleLabels[role];
          option.selected = member.role === role;
          select.appendChild(option);
        });
        select.addEventListener('change', function () {
          send(api(base, 'PUT', { username: member.username, role: select.value }));
        });
        return select;
      }

      function render(members) {
        list.textContent = '';
        members.forEach(function (member) {
          var row = document.createElement('li');
          row.className = 'people-row';
          row.dataset.username = member.username;
          var name = document.createElement('span');
          name.className = 'people-name';
          name.textContent = member.username;
          row.appendChild(name);
          row.appendChild(roleCell(member));
          if (isOwner && member.role !== 'owner') {
            var remove = document.createElement('button');
            remove.type = 'button';
            remove.className = 'btn people-remove';
            remove.textContent = 'Remove';
            remove.setAttribute('aria-label', 'Remove ' + member.username);
            remove.addEventListener('click', function () {
              send(api(base + '?username=' + encodeURIComponent(member.username), 'DELETE'));
            });
            row.appendChild(remove);
          }
          list.appendChild(row);
        });
      }

      function reload() {
        return fetchJson(base).then(render).catch(function () { fail('Could not load the members.'); });
      }

      form.onsubmit = function (e) {
        e.preventDefault();
        var field = document.getElementById('people-add-name');
        send(api(base, 'PUT', {
          username: field.value.trim(),
          role: document.getElementById('people-add-role').value,
        }).then(function (res) {
          if (res.ok) field.value = '';
          return res;
        }));
      };
      leave.onclick = function () {
        api(base + '?username=' + encodeURIComponent(account), 'DELETE').then(function (res) {
          if (res.ok) { window.location.href = '/'; return; }
          fail('Could not leave the board.');
        }).catch(function () { fail('Network error.'); });
      };
      error.hidden = true;
      list.textContent = '';
      dlg.hidden = false;
      reload();
    },

    openBin: function () {
      var id = boardId();
      if (!id) return;
      var dlg = document.getElementById('bin-dialog');
      var note = document.getElementById('bin-note');
      var table = document.getElementById('bin-table');
      var rows = document.getElementById('bin-rows');
      note.textContent = 'Loading\u2026';
      table.hidden = true;
      dlg.hidden = false;
      fetch('/api/boards/' + encodeURIComponent(id) + '/bin', {
        headers: { 'Accept': 'application/json' }, credentials: 'same-origin',
      }).then(function (res) {
        if (!res.ok) throw new Error('HTTP ' + res.status);
        return res.json();
      }).then(function (bin) {
        rows.textContent = '';
        note.textContent = bin.tasks.length
          ? 'Deleted tasks can be restored for ' + bin.retention_days + ' days.'
          : 'The recycle bin is empty.';
        table.hidden = !bin.tasks.length;
        bin.tasks.forEach(function (item) {
          var tr = document.createElement('tr');
          [item.name, item.location, item.deleted_by,
            new Date(item.deleted_at).toLocaleString(undefined, { hour12: accountPrefs.hour12 }),
            new Date(item.unrestorable_after).toLocaleString(undefined, { hour12: accountPrefs.hour12 })].forEach(function (text) {
            var td = document.createElement('td');
            td.textContent = text;
            tr.appendChild(td);
          });
          var cell = document.createElement('td');
          var btn = document.createElement('button');
          btn.type = 'button';
          btn.className = 'btn';
          btn.textContent = 'Restore';
          btn.addEventListener('click', function () {
            showConfirmDialog('Restore task', 'Are you sure you want to restore the task ' + item.name + ' to ' + item.restores_to + '?', 'Restore', function () {
              api('/api/bin/' + encodeURIComponent(item.id) + '/restore', 'POST')
                .then(function (res) {
                  return drained(res).then(function () {
                    if (!res.ok) { toast('Could not restore the task.'); return; }
                    window.location.reload();
                  });
                });
            });
          });
          cell.appendChild(btn);
          if (bin.permanent_delete) {
            var del = document.createElement('button');
            del.type = 'button';
            del.className = 'btn btn-danger';
            del.textContent = 'Delete';
            del.addEventListener('click', function () {
              showConfirmDialog('Delete task',
                'Are you sure you want to permanently delete ' + item.name +
                ' from the recycle bin? This action can NOT be undone.', 'Delete', function () {
                  api('/api/bin/' + encodeURIComponent(item.id), 'DELETE')
                    .then(function (res) {
                      return drained(res).then(function () {
                        if (!res.ok) { toast('Could not delete the task.'); return; }
                        tr.remove();
                        if (!rows.children.length) {
                          table.hidden = true;
                          note.textContent = 'The recycle bin is empty.';
                        }
                      });
                    });
                });
            });
            cell.appendChild(del);
          }
          tr.appendChild(cell);
          rows.appendChild(tr);
        });
      }).catch(function () {
        note.textContent = 'Could not load the recycle bin.';
      });
    },

    toggleDark: function () {
      var on = !document.body.classList.contains('dark');
      document.body.classList.toggle('dark', on);
      this.setPref('pomb-dark', on);
      // KF-353: keep the menu's toggle switch in sync.
      var item = document.querySelector('#board-menu [data-bm="dark"]');
      if (item) item.setAttribute('aria-checked', on ? 'true' : 'false');
    },
    toggleLargeNames: function () {
      var on = !document.body.classList.contains('large-names');
      document.body.classList.toggle('large-names', on);
      this.setPref('pomb-large-names', on);
      // KF-353: keep the menu's toggle switch in sync.
      var item = document.querySelector('#board-menu [data-bm="large-names"]');
      if (item) item.setAttribute('aria-checked', on ? 'true' : 'false');
    },
    // The Color legend is a per-board Menu toggle. The server always renders
    // the footer, hidden when the legend is off, so persist the new state,
    // then show or hide the footer in place.
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
          var footer = document.querySelector('.color-legend');
          if (footer) footer.hidden = !on;
        })
        .catch(function () {
          if (item) item.setAttribute('aria-checked', on ? 'false' : 'true');
          toast('Could not save the legend setting.');
        });
    },
    // KF-379: the help dialog, from the top-bar "?" and Menu > Help &
    // feedback.
    openHelp: function () {
      var dlg = document.getElementById('help-dialog');
      if (!dlg) { toast('Help is not available.'); return false; }
      this.showHelpTab('quick');
      dlg.hidden = false;
      var search = document.getElementById('help-search');
      if (search) search.focus();
      return true;
    },
    showHelpTab: function (name) {
      document.querySelectorAll('[data-help-tab]').forEach(function (tab) {
        tab.classList.toggle('help-tab--active', tab.getAttribute('data-help-tab') === name);
      });
      document.querySelectorAll('[data-help-panel]').forEach(function (panel) {
        panel.hidden = panel.getAttribute('data-help-panel') !== name;
      });
    },
    initHelp: function () {
      var self = this;
      var btn = document.getElementById('help-btn');
      if (btn) btn.addEventListener('click', function () { self.openHelp(); });
      var refreshTips = function () {
        var tips = document.getElementById('help-tips');
        if (tips && !tips.querySelector('.help-tip')) { tips.remove(); tips = null; }
        if (btn) btn.classList.toggle('has-tips', !!tips);
      };
      refreshTips();
      document.querySelectorAll('.help-tip-dismiss').forEach(function (button) {
        button.addEventListener('click', function () {
          var tip = button.closest('.help-tip');
          fetch('/api/settings', {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            credentials: 'same-origin',
            body: JSON.stringify({ dismiss_hint: tip.dataset.hint }),
          }).then(function (r) {
            if (!r.ok) throw new Error('HTTP ' + r.status);
            tip.remove();
            refreshTips();
          }).catch(function (ex) {
            toast('Could not dismiss the tip: ' + ex.message);
          });
        });
      });
      // KF-475: only a click that starts and ends on the backdrop closes,
      // so a drag between the dialog and the backdrop keeps it open.
      var dlg = document.getElementById('help-dialog');
      var downOnBackdrop = false;
      var upOnBackdrop = false;
      if (dlg) dlg.addEventListener('pointerdown', function (e) {
        downOnBackdrop = e.target === dlg;
      });
      if (dlg) dlg.addEventListener('pointerup', function (e) {
        upOnBackdrop = e.target === dlg;
      });
      if (dlg) dlg.addEventListener('click', function (e) {
        if (e.target === dlg && downOnBackdrop && upOnBackdrop) dlg.hidden = true;
        downOnBackdrop = false;
        upOnBackdrop = false;
      });
      document.querySelectorAll('[data-help-tab]').forEach(function (tab) {
        tab.addEventListener('click', function () { self.showHelpTab(tab.getAttribute('data-help-tab')); });
      });
      var search = document.getElementById('help-search');
      if (search) search.addEventListener('input', function () {
        var q = search.value.trim().toLowerCase();
        var shown = 0;
        document.querySelectorAll('.help-subject').forEach(function (subject) {
          var match = !q || subject.textContent.toLowerCase().indexOf(q) !== -1;
          subject.hidden = !match;
          if (match) shown++;
        });
        document.getElementById('help-empty').hidden = shown > 0;
      });
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
        var epics = document.getElementById('epics-panel');
        if (epics && !epics.hidden) self.closeEpics();
      });
    },
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

  var Notifications = {
    PAGE: 20,
    BADGE_MAX: 99,
    unread: 0,
    generation: 0,
    nextBefore: null,
    EVENT_TEXT: {
      task_assigned: function (n) { return 'assigned you to ' + n.task_name; },
      comment_added: function (n) { return 'commented on ' + n.task_name; },
      task_moved: function (n) { return 'moved ' + n.task_name + ' to ' + n.detail; },
      task_deleted: function (n) { return 'deleted ' + n.task_name; },
      column_renamed: function (n) { return 'renamed a column on ' + n.board_name + ' to ' + n.detail; },
      column_deleted: function (n) { return 'deleted the column ' + n.detail + ' on ' + n.board_name; },
      board_deleted: function (n) { return 'deleted the board ' + n.board_name; },
    },

    setUnread: function (count) {
      this.unread = count;
      TimerUI.syncBell();
    },

    badgeText: function () {
      if (this.unread <= 0) return '1';
      return this.unread > this.BADGE_MAX ? this.BADGE_MAX + '+' : String(this.unread);
    },

    actorName: function (n) {
      return n.token_name || n.actor;
    },

    describe: function (n) {
      var text = this.EVENT_TEXT[n.kind];
      return text ? text(n) : n.kind;
    },

    linkFor: function (n) {
      if (n.kind === 'board_deleted') return null;
      if (n.kind === 'task_deleted') return null;
      var base = '/b/' + encodeURIComponent(n.board_id);
      if (n.task_id) return { href: base + '#task-' + encodeURIComponent(n.task_id), label: 'VIEW TASK' };
      return { href: base, label: 'VIEW BOARD' };
    },

    ago(at, now) {
      const seconds = Math.floor((now - at) / 1000);
      if (seconds < 60) return 'just now';
      if (seconds < 3600) return Math.floor(seconds / 60) + 'm ago';
      if (seconds < 86400) return Math.floor(seconds / 3600) + 'h ago';
      const days = Math.floor(seconds / 86400);
      return days === 1 ? 'yesterday' : days + 'd ago';
    },

    row: function (n) {
      var row = document.createElement('div');
      row.className = 'notification-row' + (n.read ? '' : ' notification-row--unread');
      row.setAttribute('data-notification-id', String(n.id));
      var text = document.createElement('div');
      text.className = 'notification-text';
      var who = document.createElement('strong');
      who.textContent = this.actorName(n);
      text.appendChild(who);
      text.appendChild(document.createTextNode(' ' + this.describe(n)));
      row.appendChild(text);
      const dismiss = document.createElement('button');
      dismiss.className = 'notification-dismiss';
      dismiss.setAttribute('type', 'button');
      dismiss.setAttribute('aria-label', 'Dismiss notification');
      dismiss.title = 'Dismiss';
      dismiss.textContent = '×';
      row.appendChild(dismiss);
      const at = Date.parse(n.created_at);
      if (!isNaN(at)) {
        const now = Date.now();
        const when = document.createElement('time');
        when.className = 'notification-time';
        when.setAttribute('datetime', n.created_at);
        when.title = LocalTime.write('datetime', at, now, undefined, accountPrefs.hour12);
        when.textContent = this.ago(at, now);
        row.appendChild(when);
      }
      var link = this.linkFor(n);
      if (link) {
        var a = document.createElement('a');
        a.className = 'notification-link';
        a.href = link.href;
        a.textContent = link.label;
        row.appendChild(a);
      }
      return row;
    },

    markRead: function (ids) {
      var self = this;
      return Promise.all(ids.map(function (id) {
        return fetch('/api/notifications/' + id + '/read', { method: 'POST', headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
          .then(function (res) { return jsonIfJson(res); });
      })).then(function (results) {
        var counts = results.filter(Boolean).map(function (r) { return r.unread; });
        if (counts.length) self.setUnread(Math.min.apply(null, counts));
      });
    },

    load: function (list, empty, more, before) {
      var self = this;
      var generation = this.generation;
      var url = '/api/notifications?limit=' + this.PAGE + (before ? '&before=' + before : '');
      return fetch(url, { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then(function (res) { return jsonIfJson(res); })
        .then(function (page) {
          if (!page || generation !== self.generation) return;
          var unseen = [];
          page.items.forEach(function (n) {
            list.appendChild(self.row(n));
            if (!n.read) unseen.push(n.id);
          });
          self.nextBefore = page.next_before;
          empty.hidden = list.children.length > 0;
          more.hidden = page.next_before === null;
          self.setUnread(page.unread);
          if (unseen.length) return self.markRead(unseen);
        });
    },

    markAll: function (list) {
      var self = this;
      return fetch('/api/notifications/read', { method: 'POST', headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then(function (res) { return jsonIfJson(res); })
        .then(function (done) {
          if (!done) return;
          self.setUnread(done.unread);
          list.querySelectorAll('.notification-row--unread').forEach(function (row) {
            row.classList.remove('notification-row--unread');
          });
        });
    },

    dismiss(id, row, list, empty) {
      return fetch('/api/notifications/' + id, { method: 'DELETE', headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then((res) => jsonIfJson(res))
        .then((done) => {
          if (!done) return;
          row.remove();
          this.setUnread(done.unread);
          empty.hidden = list.children.length > 0;
        });
    },

    clearAll(list, empty, more) {
      return fetch('/api/notifications', { method: 'DELETE', headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
        .then((res) => jsonIfJson(res))
        .then((done) => {
          if (!done) return;
          this.generation++;
          list.textContent = '';
          empty.hidden = false;
          more.hidden = true;
          this.nextBefore = null;
          this.setUnread(done.unread);
        });
    },

    openTask: function (link) {
      var url = new URL(link.href, window.location.href);
      if (url.pathname !== window.location.pathname) return false;
      history.replaceState(null, '', url.pathname + url.hash);
      window.dispatchEvent(new HashChangeEvent('hashchange'));
      return true;
    },
  };

  function initNotificationsPanel() {
    var btn = document.getElementById('notifications-btn');
    var panel = document.getElementById('notifications-panel');
    if (!btn || !panel) return;
    var list = panel.querySelector('.notifications-list');
    var empty = panel.querySelector('.notifications-empty');
    var more = panel.querySelector('.notifications-more');
    var markAll = panel.querySelector('.notifications-mark-all');
    const clearAll = panel.querySelector('.notifications-clear-all');
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
    function refresh() {
      list.textContent = '';
      Notifications.generation++;
      Notifications.load(list, empty, more, null);
    }
    btn.addEventListener('click', function (e) {
      e.stopPropagation();
      closeAccountMenu();
      if (panel.hidden) {
        placePanel();
        panel.hidden = false;
        btn.setAttribute('aria-expanded', 'true');
        refresh();
      } else {
        closePanel();
      }
    });
    markAll.addEventListener('click', function () { Notifications.markAll(list); });
    clearAll.addEventListener('click', () => Notifications.clearAll(list, empty, more));
    more.addEventListener('click', function () {
      Notifications.load(list, empty, more, Notifications.nextBefore);
    });
    list.addEventListener('click', function (e) {
      const dismiss = e.target.closest('.notification-dismiss');
      if (dismiss) {
        const row = dismiss.closest('.notification-row');
        Notifications.dismiss(row.getAttribute('data-notification-id'), row, list, empty);
        return;
      }
      var link = e.target.closest('.notification-link');
      if (link && Notifications.openTask(link)) {
        e.preventDefault();
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
    fetch('/api/notifications?limit=1', { headers: { 'Accept': 'application/json' }, credentials: 'same-origin' })
      .then(function (res) { return jsonIfJson(res); })
      .then(function (page) { if (page) Notifications.setUnread(page.unread); });
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
      var raw = localStorage.getItem('pomb.cardsec.' + taskId);
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
    showPendingToast();
    initTaskSortable();
    initSelection();
    initColumnSortable();
    applyCollapsedColumns();
    initAddTask();
    initBoardMenus();
    initMembersDialog();
    initBoardsDropdown();
    initAccountMenu(); // KF-195
    initNotificationsPanel(); // KF-310
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
        if (sec === 'comments' || sec === 'description') {
          if (taskId) openModal(taskId);
          return;
        }
        var body = card.querySelector('[data-sec-body="' + sec + '"]');
        if (!body || !taskId) return;
        var st = cardSecState(taskId);
        st[sec] = body.hidden; // hidden now => shown after toggle
        body.hidden = !st[sec];
        try { localStorage.setItem('pomb.cardsec.' + taskId, JSON.stringify(st)); } catch (e2) {}
        return;
      }
      // "N hidden (M done)" expands the full inline subtask list, and the
      // label turns into "Hide" until it folds again (KF-364).
      var more = e.target.closest('.card-sub-more');
      if (more) toggleSubFold(more);
    });

    // KF-478: Enter and Space work the folded-subtask label like a click.
    document.addEventListener('keydown', function (e) {
      if ((e.key !== 'Enter' && e.key !== ' ') || !e.target.classList ||
          !e.target.classList.contains('card-sub-more')) return;
      e.preventDefault();
      toggleSubFold(e.target);
    });

    function toggleSubFold(more) {
      var subsec = more.closest('.card-subsec');
      if (!subsec) return;
      var open = subsec.classList.toggle('card-sub-expanded');
      if (open) {
        more.dataset.summary = more.textContent;
        more.textContent = 'Hide';
      } else {
        more.textContent = more.dataset.summary || more.textContent;
      }
      more.title = open ? 'Click to hide done subtasks' : 'Click to expand subtasks';
    }

    // KF-478: folds a card's done subtasks the way a render does (a list
    // longer than the server's CARD_SUBTASKS_SHOWN with any done item), so
    // the "N hidden (N done)" label matches a reload.
    function refoldSubtasks(card) {
      var subsec = card.querySelector('.card-subsec');
      if (!subsec || subsec.dataset.subAll !== 'true') return;
      var items = subsec.querySelectorAll('.subTasks > li');
      var done = subsec.querySelectorAll('.card-sub-check:checked').length;
      var fold = items.length > 3 && done > 0;
      Array.prototype.forEach.call(items, function (li) {
        var box = li.querySelector('.card-sub-check');
        li.classList.toggle('card-sub-extra', fold && !!box && box.checked);
      });
      var more = subsec.querySelector('.card-sub-more');
      if (!fold) {
        if (more) more.remove();
        subsec.classList.remove('card-sub-expanded');
        return;
      }
      if (!more) {
        more = document.createElement('div');
        more.className = 'card-sub-more';
        more.title = 'Click to expand subtasks';
        more.setAttribute('role', 'button');
        more.tabIndex = 0;
        more.textContent = 'x';
        subsec.appendChild(more);
      }
      var summary = done + ' hidden (' + done + ' done)';
      if (subsec.classList.contains('card-sub-expanded')) more.dataset.summary = summary;
      else more.textContent = summary;
    }

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
          var fill = card.querySelector('.card-progress-fill');
          if (fill) fill.style.width = Math.floor(done * 100 / total) + '%';
          var shown = card.querySelector('.card-progress-text');
          if (shown) shown.textContent = done + ' of ' + total;
          var li = check.closest('li');
          if (li) li.classList.toggle('card-sub-done', check.checked);
          refoldSubtasks(card);
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
      var face = e.target.closest('.card-desc');
      if (face) {
        e.preventDefault();
        var faceCard = face.closest('.task-card');
        if (faceCard && faceCard.dataset.taskId) openModal(faceCard.dataset.taskId);
        return;
      }
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

  var Finder = {
    RECENT_KEY: 'pomb-finder-recent',
    RECENT_MAX_ENTRIES: 10,
    GONE_STATUSES: [403, 404],
    DEBOUNCE_MS: 120,
    SEARCH_LIMIT_HITS: 25,
    FLASH_MS: 2400,
    EXCERPT_CHARS: 400,
    PREVIEW_SUBTASKS: 8,
    PREVIEW_COMMENTS: 3,
    HASH_FIND: '#find-',
    HASH_TASK: '#task-',
    STATUS_LABELS: { open: 'Open', done: 'Done', archived: 'Archived' },
    MATCH: { TITLE: 'title', DESCRIPTION: 'description', SUBTASKS: 'subtasks', COMMENTS: 'comments' },
    FIELD_LABELS: { title: 'title', description: 'description', subtasks: 'subtask', comments: 'comment' },
    ready: false,
    pending: false,
    queued: null,
    showingRecent: false,
    chip: null,
    terms: [],
    choices: {},
    menuKind: null,
    TERM_KEYS: ['in', 'board', 'column', 'label', 'color', 'is'],
    NOT_ARCHIVED: '-is:archived',
    hits: [],
    index: -1,
    seq: 0,
    previewSeq: 0,
    timer: null,
    opener: null,
    cache: {},

    init: function () {
      if (this.ready) return;
      this.root = document.getElementById('finder');
      if (!this.root) return;
      this.ready = true;
      var self = this;
      this.input = document.getElementById('finder-input');
      this.chipsEl = document.getElementById('finder-chips');
      this.body = document.getElementById('finder-body');
      this.help = document.getElementById('finder-help');
      this.helpBtn = document.getElementById('finder-help-btn');
      this.archivedBtn = document.getElementById('finder-archived-btn');
      this.list = document.getElementById('finder-list');
      this.preview = document.getElementById('finder-preview');
      this.input.addEventListener('input', function () {
        self.hideHelp();
        self.closeMenus();
        self.absorb();
        window.clearTimeout(self.timer);
        self.pending = true;
        self.queued = null;
        self.seq += 1;
        self.timer = window.setTimeout(function () { self.search(); }, self.DEBOUNCE_MS);
      });
      this.root.addEventListener('keydown', function (e) { self.onKey(e); });
      this.root.addEventListener('mousedown', function (e) {
        if (e.target === self.root) self.close();
      });
      this.root.querySelector('[data-finder-close]').addEventListener('click', function () { self.close(); });
      this.helpBtn.addEventListener('click', function () { self.toggleHelp(); });
      this.archivedBtn.addEventListener('click', function () { self.toggleTerm(self.NOT_ARCHIVED); });
      this.root.querySelectorAll('[data-finder-menu]').forEach(function (btn) {
        btn.addEventListener('click', function () { self.toggleMenu(btn.getAttribute('data-finder-menu')); });
      });
      this.root.addEventListener('click', function (e) {
        if (!e.target.closest('.finder-filter-wrap')) self.closeMenus();
      });
      this.list.addEventListener('mousemove', function (e) {
        var row = e.target.closest('[data-finder-index]');
        if (row) self.select(Number(row.getAttribute('data-finder-index')), false);
      });
      this.list.addEventListener('click', function (e) {
        var row = e.target.closest('[data-finder-index]');
        if (!row) return;
        self.select(Number(row.getAttribute('data-finder-index')), false);
        self.activate(e.shiftKey);
      });
      document.querySelectorAll('[data-finder-open]').forEach(function (btn) {
        btn.addEventListener('click', function () { self.open(); });
      });
      document.addEventListener('keydown', function (e) {
        if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === 'k') {
          e.preventDefault();
          if (e.repeat) return;
          if (self.root.hidden) self.open(); else self.close();
        } else if (e.key === '/' && !e.metaKey && !e.ctrlKey && !e.altKey && self.root.hidden && !self.typing(e.target)) {
          e.preventDefault();
          self.open();
        }
      });
      this.flashFromHash();
    },

    typing: function (el) {
      return Boolean(el && el.closest && el.closest('input, textarea, select, [contenteditable=""], [contenteditable="true"]'));
    },

    open: function () {
      if (!this.ready || !this.root.hidden) return;
      this.opener = document.activeElement;
      this.cache = {};
      var current = boardId();
      var title = document.querySelector('.board-bar-title');
      this.chip = current ? { id: current, name: title ? title.textContent.trim() : current } : null;
      this.terms = [];
      this.choices = {};
      this.paintChips();
      this.hideHelp();
      this.closeMenus();
      this.input.value = '';
      this.root.hidden = false;
      this.input.focus();
      this.search();
    },

    close: function () {
      if (this.root.hidden) return;
      window.clearTimeout(this.timer);
      this.pending = false;
      this.queued = null;
      this.seq += 1;
      this.previewSeq += 1;
      this.closeMenus();
      this.root.hidden = true;
      var opener = this.opener;
      this.opener = null;
      if (opener && opener.isConnected && opener.focus) opener.focus();
    },

    paintChips: function () {
      var self = this;
      this.chipsEl.textContent = '';
      if (this.chip) {
        this.chipsEl.appendChild(this.chipNode('board:' + this.chip.name, false, function () {
          self.chip = null;
          self.changed();
        }));
      }
      this.terms.forEach(function (term) {
        self.chipsEl.appendChild(self.chipNode(term, term.charAt(0) === '-', function () { self.toggleTerm(term); }));
      });
      this.archivedBtn.setAttribute('aria-pressed', this.hasTerm(this.NOT_ARCHIVED) ? 'false' : 'true');
    },

    chipNode: function (label, negated, remove) {
      var chip = document.createElement('span');
      chip.className = 'finder-chip' + (negated ? ' finder-chip--negated' : '');
      var text = document.createElement('span');
      text.className = 'finder-chip-text';
      text.textContent = label;
      chip.appendChild(text);
      var x = document.createElement('button');
      x.type = 'button';
      x.className = 'finder-chip-x';
      x.setAttribute('aria-label', 'Remove ' + label);
      x.textContent = '×';
      x.addEventListener('click', remove);
      chip.appendChild(x);
      return chip;
    },

    changed: function () {
      window.clearTimeout(this.timer);
      this.queued = null;
      this.paintChips();
      this.closeMenus();
      this.input.focus();
      this.search();
    },

    hasTerm: function (term) {
      var wanted = term.toLowerCase();
      return this.terms.some(function (t) { return t.toLowerCase() === wanted; });
    },

    toggleTerm: function (term) {
      var wanted = term.toLowerCase();
      var kept = this.terms.filter(function (t) { return t.toLowerCase() !== wanted; });
      this.terms = kept.length === this.terms.length ? kept.concat([term]) : kept;
      this.changed();
    },

    tokens: function (text) {
      var out = [];
      var cur = '';
      var quoted = false;
      for (var i = 0; i < text.length; i++) {
        var ch = text.charAt(i);
        if (ch === '"') quoted = !quoted;
        if (!quoted && /\s/.test(ch)) {
          if (cur) { out.push({ text: cur, closed: true }); cur = ''; }
        } else {
          cur += ch;
        }
      }
      if (cur) out.push({ text: cur, closed: false });
      return out;
    },

    isTerm: function (token) {
      var negated = token.charAt(0) === '-';
      var body = negated ? token.slice(1) : token;
      if (!body) return false;
      if (negated) return true;
      if (body.charAt(0) === '"') return body.length > 2 && body.charAt(body.length - 1) === '"';
      var at = body.indexOf(':');
      return at > 0 && at < body.length - 1 && this.TERM_KEYS.indexOf(body.slice(0, at).toLowerCase()) !== -1;
    },

    absorb: function () {
      if (this.input.selectionStart !== this.input.value.length) return;
      var self = this;
      var kept = [];
      var moved = false;
      this.tokens(this.input.value).forEach(function (token) {
        if (token.closed && self.isTerm(token.text)) {
          if (!self.hasTerm(token.text)) self.terms.push(token.text);
          moved = true;
        } else {
          kept.push(token);
        }
      });
      if (!moved) return;
      this.input.value = kept.map(function (token) { return token.closed ? token.text + ' ' : token.text; }).join('');
      this.paintChips();
    },

    quote: function (name) {
      return /[\s"]/.test(name) ? '"' + name.replace(/"/g, '') + '"' : name;
    },

    toggleHelp: function () {
      if (this.help.hidden) this.showHelp(); else this.hideHelp();
    },

    showHelp: function () {
      this.closeMenus();
      this.help.hidden = false;
      this.body.hidden = true;
      this.helpBtn.setAttribute('aria-expanded', 'true');
    },

    hideHelp: function () {
      this.help.hidden = true;
      this.body.hidden = false;
      this.helpBtn.setAttribute('aria-expanded', 'false');
    },

    closeMenus: function () {
      this.root.querySelectorAll('[data-finder-menu]').forEach(function (btn) {
        btn.setAttribute('aria-expanded', 'false');
        document.getElementById('finder-menu-' + btn.getAttribute('data-finder-menu')).hidden = true;
      });
      this.menuKind = null;
    },

    toggleMenu: function (kind) {
      var wasOpen = this.menuKind === kind;
      this.closeMenus();
      if (wasOpen) return;
      this.hideHelp();
      this.menuKind = kind;
      var btn = this.root.querySelector('[data-finder-menu="' + kind + '"]');
      var menu = document.getElementById('finder-menu-' + kind);
      btn.setAttribute('aria-expanded', 'true');
      menu.hidden = false;
      this.fillMenu(menu, kind, null);
      var self = this;
      this.choicesFor(kind).then(function (names) {
        if (self.menuKind !== kind) return;
        self.fillMenu(menu, kind, names);
      }).catch(function () {
        if (self.menuKind !== kind) return;
        self.choices = {};
        self.fillMenu(menu, kind, []);
      });
    },

    choicesFor: function (kind) {
      var key = kind + ':' + (this.chip ? this.chip.id : '*');
      if (!this.choices[key]) this.choices[key] = this.loadChoices(kind);
      return this.choices[key];
    },

    loadChoices: function (kind) {
      var boards = this.chip ? Promise.resolve([{ id: this.chip.id }]) : fetchJson('/api/boards');
      return boards.then(function (list) {
        return Promise.all(list.map(function (board) {
          return fetchJson('/api/boards/' + encodeURIComponent(board.id) + (kind === 'column' ? '/columns' : '/colors'));
        }));
      }).then(function (groups) {
        var seen = {};
        var names = [];
        groups.forEach(function (items) {
          items.forEach(function (item) {
            var name = kind === 'column' ? item.name : (item.enabled === false ? '' : item.label);
            if (!name || seen[name.toLowerCase()]) return;
            seen[name.toLowerCase()] = true;
            names.push(name);
          });
        });
        return names;
      });
    },

    fillMenu: function (menu, kind, names) {
      var self = this;
      menu.textContent = '';
      if (names === null || !names.length) {
        var note = document.createElement('p');
        note.className = 'finder-note';
        note.textContent = names === null ? 'Loading.' : 'Nothing to pick.';
        menu.appendChild(note);
        return;
      }
      names.forEach(function (name) {
        var term = kind + ':' + self.quote(name);
        var item = document.createElement('button');
        item.type = 'button';
        item.className = 'finder-menu-item';
        item.setAttribute('role', 'option');
        item.setAttribute('aria-selected', self.hasTerm(term) ? 'true' : 'false');
        item.textContent = name;
        item.addEventListener('click', function () { self.toggleTerm(term); });
        menu.appendChild(item);
      });
    },

    onKey: function (e) {
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        this.close();
      } else if (e.target !== this.input && e.target.closest('button') && (e.key === 'Enter' || e.key === ' ')) {
        return;
      } else if (!this.help.hidden && (e.key === 'Enter' || e.key === 'ArrowDown' || e.key === 'ArrowUp')) {
        e.preventDefault();
        this.hideHelp();
      } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault();
        if (!this.hits.length) return;
        var step = e.key === 'ArrowDown' ? 1 : -1;
        this.select((this.index + step + this.hits.length) % this.hits.length, true);
      } else if (e.key === 'Enter') {
        e.preventDefault();
        if (this.pending) { this.queued = { highlight: e.shiftKey }; return; }
        this.activate(e.shiftKey);
      } else if (e.key === 'Backspace' && e.target === this.input && (this.terms.length || this.chip) && this.input.value === '') {
        e.preventDefault();
        if (this.terms.length) this.terms.pop(); else this.chip = null;
        this.changed();
      }
    },

    query: function () {
      var text = this.input.value.trim();
      var parts = [];
      if (this.chip) parts.push('board:' + this.chip.id);
      parts = parts.concat(this.terms);
      if (text) parts.push(text);
      return { text: text, filtered: this.terms.length > 0, q: parts.join(' ') };
    },

    search: function () {
      var self = this;
      this.pending = true;
      this.showingRecent = false;
      var query = this.query();
      this.seq += 1;
      var seq = this.seq;
      if (!query.text && !query.filtered) {
        var recent = this.recent().filter(function (hit) {
          return !self.chip || hit.board_id === self.chip.id;
        });
        if (recent.length) {
          this.showingRecent = true;
          this.show(recent, 'Recent');
          this.settle();
          return;
        }
        query.q = (this.chip ? 'board:' + this.chip.id + ' ' : '') + 'is:open';
      }
      fetch('/api/search?limit=' + this.SEARCH_LIMIT_HITS + '&q=' + encodeURIComponent(query.q), {
        headers: { 'Accept': 'application/json' },
        credentials: 'same-origin',
      })
        .then(function (res) {
          if (res.ok) return res.json();
          return res.json().catch(function () { return {}; }).then(function (body) {
            throw new Error(body && body.message ? body.message : 'Search failed.');
          });
        })
        .then(function (page) {
          if (seq !== self.seq) return;
          self.show(page.items || [], null, page.truncated);
          self.settle();
        })
        .catch(function (err) {
          if (seq !== self.seq) return;
          self.fail(err.message);
          self.settle();
        });
    },

    settle: function () {
      var queued = this.queued;
      this.pending = false;
      this.queued = null;
      if (queued && this.hits.length && this.help.hidden) this.activate(queued.highlight);
    },

    fail: function (message) {
      this.hits = [];
      this.index = -1;
      this.list.textContent = '';
      var note = document.createElement('p');
      note.className = 'finder-note';
      note.textContent = message;
      this.list.appendChild(note);
      this.preview.textContent = '';
    },

    show: function (hits, heading, truncated) {
      this.hits = hits;
      this.index = -1;
      this.list.textContent = '';
      if (!hits.length) {
        var note = document.createElement('p');
        note.className = 'finder-note';
        note.textContent = 'No matching cards.';
        this.list.appendChild(note);
        this.index = -1;
        this.preview.textContent = '';
        return;
      }
      var last = null;
      for (var i = 0; i < hits.length; i++) {
        var group = heading || this.STATUS_LABELS[hits[i].status] || hits[i].status;
        if (group !== last) {
          var h = document.createElement('div');
          h.className = 'finder-group';
          h.textContent = group;
          h.setAttribute('role', 'presentation');
          this.list.appendChild(h);
          last = group;
        }
        this.list.appendChild(this.row(hits[i], i));
      }
      if (truncated) {
        var more = document.createElement('p');
        more.className = 'finder-note';
        more.textContent = 'More cards match. Keep typing to narrow the list.';
        this.list.appendChild(more);
      }
      this.select(0, true);
    },

    row: function (hit, i) {
      var row = document.createElement('div');
      row.className = 'finder-row';
      row.id = 'finder-row-' + i;
      row.setAttribute('role', 'option');
      row.setAttribute('data-finder-index', String(i));
      var name = document.createElement('div');
      name.className = 'finder-row-name';
      this.mark(name, hit.name, hit.matched_field === this.MATCH.TITLE ? hit.ranges : []);
      row.appendChild(name);
      var meta = document.createElement('div');
      meta.className = 'finder-row-meta';
      var where = hit.board_name + ' / ' + hit.column_name;
      if (hit.matched_field && hit.matched_field !== this.MATCH.TITLE) {
        where += ' - in ' + this.FIELD_LABELS[hit.matched_field];
      }
      meta.textContent = where;
      row.appendChild(meta);
      return row;
    },

    mark: function (host, text, ranges) {
      var chars = Array.from(text);
      var at = 0;
      (ranges || []).forEach(function (r) {
        if (r.start > at) host.appendChild(document.createTextNode(chars.slice(at, r.start).join('')));
        var m = document.createElement('mark');
        m.textContent = chars.slice(r.start, r.end).join('');
        host.appendChild(m);
        at = r.end;
      });
      if (at < chars.length) host.appendChild(document.createTextNode(chars.slice(at).join('')));
    },

    select: function (i, scroll) {
      if (i === this.index) return;
      var prev = this.list.querySelector('.finder-row--selected');
      if (prev) {
        prev.classList.remove('finder-row--selected');
        prev.setAttribute('aria-selected', 'false');
      }
      this.index = i;
      var row = this.list.querySelector('[data-finder-index="' + i + '"]');
      if (!row) return;
      row.classList.add('finder-row--selected');
      row.setAttribute('aria-selected', 'true');
      this.input.setAttribute('aria-activedescendant', row.id);
      if (scroll) row.scrollIntoView({ block: 'nearest' });
      this.showPreview(this.hits[i]);
    },

    excerpt: function (text, ranges, max) {
      var chars = Array.from(text);
      if (chars.length <= max) return { text: text, ranges: ranges, lead: false, trail: false };
      var first = ranges.length ? ranges[0].start : 0;
      var from = Math.max(0, Math.min(first - Math.floor(max / 4), chars.length - max));
      var to = from + max;
      var kept = [];
      ranges.forEach(function (r) {
        if (r.end <= from || r.start >= to) return;
        kept.push({ start: Math.max(r.start, from) - from, end: Math.min(r.end, to) - from });
      });
      return { text: chars.slice(from, to).join(''), ranges: kept, lead: from > 0, trail: to < chars.length };
    },

    block: function (label, text, hit, field, index) {
      var wrap = document.createElement('div');
      wrap.className = 'finder-pv-block';
      var head = document.createElement('div');
      head.className = 'finder-pv-label';
      head.textContent = label;
      wrap.appendChild(head);
      var body = document.createElement('div');
      body.className = 'finder-pv-text';
      var hot = hit.matched_field === field && (index === null || hit.matched_index === index);
      var cut = this.excerpt(text, hot ? hit.ranges : [], this.EXCERPT_CHARS);
      if (cut.lead) body.appendChild(document.createTextNode('…'));
      this.mark(body, cut.text, cut.ranges);
      if (cut.trail) body.appendChild(document.createTextNode('…'));
      wrap.appendChild(body);
      return wrap;
    },

    showPreview: function (hit) {
      var self = this;
      this.previewSeq += 1;
      var seq = this.previewSeq;
      this.preview.textContent = '';
      if (!hit) return;
      var title = document.createElement('div');
      title.className = 'finder-pv-title';
      this.mark(title, hit.name, hit.matched_field === this.MATCH.TITLE ? hit.ranges : []);
      this.preview.appendChild(title);
      var meta = document.createElement('div');
      meta.className = 'finder-pv-meta';
      meta.textContent = hit.board_name + ' / ' + hit.column_name + ' - ' + (this.STATUS_LABELS[hit.status] || hit.status);
      this.preview.appendChild(meta);
      var cached = this.cache[hit.task_id];
      var pending = cached || Promise.all([
        fetchJson('/api/tasks/' + encodeURIComponent(hit.task_id)),
        fetchJson('/api/tasks/' + encodeURIComponent(hit.task_id) + '/comments'),
      ]);
      this.cache[hit.task_id] = pending;
      pending.then(function (parts) {
        if (seq !== self.previewSeq) return;
        self.fillPreview(hit, parts[0], parts[1]);
      }).catch(function () {
        delete self.cache[hit.task_id];
        if (seq !== self.previewSeq) return;
        var note = document.createElement('p');
        note.className = 'finder-note';
        note.textContent = 'Could not load the card.';
        self.preview.appendChild(note);
      });
    },

    fillPreview: function (hit, task, comments) {
      var self = this;
      if (task.description) {
        this.preview.appendChild(this.block('Description', task.description, hit, this.MATCH.DESCRIPTION, null));
      }
      var subtasks = task.subtasks || [];
      if (subtasks.length) {
        var shown = subtasks.map(function (sub, at) { return at; }).slice(0, this.PREVIEW_SUBTASKS);
        if (hit.matched_field === this.MATCH.SUBTASKS && shown.indexOf(hit.matched_index) === -1) {
          shown[shown.length - 1] = hit.matched_index;
        }
        var wrap = document.createElement('div');
        wrap.className = 'finder-pv-block';
        var head = document.createElement('div');
        head.className = 'finder-pv-label';
        head.textContent = 'Subtasks';
        wrap.appendChild(head);
        shown.forEach(function (at) {
          var sub = subtasks[at];
          var line = document.createElement('div');
          line.className = 'finder-pv-sub' + (sub.done ? ' finder-pv-sub--done' : '');
          var hot = hit.matched_field === self.MATCH.SUBTASKS && hit.matched_index === at;
          self.mark(line, sub.name, hot ? hit.ranges : []);
          wrap.appendChild(line);
        });
        if (subtasks.length > shown.length) {
          var rest = document.createElement('div');
          rest.className = 'finder-pv-more';
          rest.textContent = (subtasks.length - shown.length) + ' more';
          wrap.appendChild(rest);
        }
        this.preview.appendChild(wrap);
      }
      if (comments.length) {
        var picks = comments.map(function (c, at) { return at; }).slice(-this.PREVIEW_COMMENTS);
        if (hit.matched_field === this.MATCH.COMMENTS && picks.indexOf(hit.matched_index) === -1) {
          picks = [hit.matched_index].concat(picks.slice(1));
        }
        picks.sort(function (a, b) { return a - b; });
        picks.forEach(function (at) {
          var comment = comments[at];
          if (!comment) return;
          self.preview.appendChild(self.block('Comment by ' + comment.author, comment.body, hit, self.MATCH.COMMENTS, at));
        });
      }
    },

    recent: function () {
      try {
        var list = JSON.parse(window.localStorage.getItem(this.RECENT_KEY) || '[]');
        return Array.isArray(list) ? list : [];
      } catch (err) {
        return [];
      }
    },

    remember: function (hit) {
      var entry = {
        task_id: hit.task_id,
        name: hit.name,
        board_id: hit.board_id,
        board_name: hit.board_name,
        column_id: hit.column_id,
        column_name: hit.column_name,
        status: hit.status,
        matched_field: this.MATCH.TITLE,
        matched_index: null,
        ranges: [],
      };
      var list = this.recent().filter(function (old) { return old.task_id !== entry.task_id; });
      list.unshift(entry);
      this.saveRecent(list);
    },

    forget: function (taskId) {
      this.saveRecent(this.recent().filter(function (old) { return old.task_id !== taskId; }));
    },

    saveRecent: function (list) {
      try {
        window.localStorage.setItem(this.RECENT_KEY, JSON.stringify(list.slice(0, this.RECENT_MAX_ENTRIES)));
      } catch (err) {
        return;
      }
    },

    activate: function (highlight) {
      var hit = this.hits[this.index];
      if (!hit) return;
      if (!this.showingRecent) { this.go(hit, highlight); return; }
      var self = this;
      fetch('/api/tasks/' + encodeURIComponent(hit.task_id), {
        headers: { 'Accept': 'application/json' },
        credentials: 'same-origin',
      }).then(function (res) {
        if (self.GONE_STATUSES.indexOf(res.status) === -1) { self.go(hit, highlight); return; }
        self.forget(hit.task_id);
        toast('That card no longer exists, so it was removed from the recent list.');
        if (!self.root.hidden && self.showingRecent) self.search();
      }).catch(function () {
        self.go(hit, highlight);
      });
    },

    go: function (hit, highlight) {
      this.remember(hit);
      var onBoard = boardId() === hit.board_id;
      this.close();
      if (highlight) {
        if (onBoard) this.flash(hit.task_id);
        else window.location.href = '/b/' + encodeURIComponent(hit.board_id) + this.HASH_FIND + encodeURIComponent(hit.task_id);
      } else if (onBoard) {
        openModal(hit.task_id);
      } else {
        window.location.href = '/b/' + encodeURIComponent(hit.board_id) + this.HASH_TASK + encodeURIComponent(hit.task_id);
      }
    },

    flash: function (taskId) {
      var card = document.querySelector('.task-card[data-task-id="' + cssEscape(taskId) + '"]');
      if (!card) { toast('That card is not on the board view.'); return; }
      card.scrollIntoView({ behavior: 'smooth', block: 'center', inline: 'center' });
      card.classList.remove('finder-flash');
      void card.offsetWidth;
      card.classList.add('finder-flash');
      window.setTimeout(function () { card.classList.remove('finder-flash'); }, this.FLASH_MS);
    },

    flashFromHash: function () {
      var hash = window.location.hash || '';
      if (hash.indexOf(this.HASH_FIND) !== 0) return;
      var m = /^([\w-]+)$/.exec(hash.slice(this.HASH_FIND.length));
      if (!m) return;
      window.history.replaceState(null, '', window.location.pathname + window.location.search);
      this.flash(m[1]);
    },
  };

  // KF-357: the settings pages carry the global top bar without the board.
  var topbarOnlyInitialized = false;
  function initTopbarOnly() {
    if (topbarOnlyInitialized || document.querySelector('.board-wrap')) return;
    if (!document.querySelector('.topbar #boards-dropdown')) return;
    topbarOnlyInitialized = true;
    initBoardsDropdown();
    initAccountMenu();
    initNotificationsPanel();
  }

  document.addEventListener('DOMContentLoaded', function () {
    if (document.querySelector('.board-wrap')) initBoard();
    initTopbarOnly();
    initReportModal();
    initEntryEdit();
    initTaskExtras();
    TimerLogPage.init();
    TimerStatsPage.init();
    TimerUI.init();
    BoardChrome.init();
    initBoardDeletePage();
    initBoardGeneralActions();
    Finder.init();
  });
  // In case app.js runs after DOMContentLoaded (defer ordering):
  if (document.readyState !== 'loading') {
    if (document.querySelector('.board-wrap')) initBoard();
    initTopbarOnly();
    initReportModal();
    initEntryEdit();
    initTaskExtras();
    TimerLogPage.init();
    TimerStatsPage.init();
    TimerUI.init();
    BoardChrome.init();
    initBoardDeletePage();
    initBoardGeneralActions();
    Finder.init();
  }

  function bindCalls() {
    document.querySelectorAll('[data-call]').forEach(function (el) {
      var path = el.getAttribute('data-call').split('.');
      el.addEventListener('click', function () {
        var target = window[path[0]];
        if (!target || typeof target[path[1]] !== 'function') {
          console.error('Unknown data-call: ' + el.getAttribute('data-call'));
          return;
        }
        var arg = el.getAttribute('data-call-arg');
        if (arg === null) target[path[1]]();
        else target[path[1]](arg);
      });
    });
  }
  bindCalls();

  var MassUpdate = (function () {
    var STEPS = ['filter', 'select', 'action', 'preview'];
    var step = 0;
    var picked = {};
    var matches = [];
    var lastFilter = '';
    var request = null;
    var previewSeq = 0;
    var applying = false;
    var targetColumns = [];

    function byId(id) { return document.getElementById(id); }

    function stepSection(name) {
      return document.querySelector('#mass-update-dialog [data-mu-step="' + name + '"]');
    }

    function showError(message) {
      var box = byId('mu-error');
      box.textContent = message || '';
      box.hidden = !message;
    }

    function addOption(select, value, text) {
      var option = document.createElement('option');
      option.value = value;
      option.textContent = text;
      select.appendChild(option);
    }

    function boardColumns() {
      return Array.prototype.map.call(document.querySelectorAll('#column-headers-row .columnHeader'), function (header) {
        return { id: header.dataset.columnId, name: header.dataset.columnName, done: header.dataset.isDone === '1' };
      });
    }

    function cardLabels(card) {
      try { return JSON.parse(card.dataset.labels || '[]'); } catch (e) { return []; }
    }

    function fillFilters() {
      var columns = byId('mu-columns');
      columns.innerHTML = '';
      boardColumns().forEach(function (column) { addOption(columns, column.id, column.name); });

      var color = byId('mu-color');
      color.innerHTML = '';
      addOption(color, 'all', 'Any');
      document.querySelectorAll('#board-colors span[data-value]').forEach(function (swatch) {
        addOption(color, swatch.dataset.value, swatch.dataset.label || swatch.dataset.value);
      });

      var label = byId('mu-label');
      label.innerHTML = '';
      addOption(label, 'all', 'Any');
      var seen = {};
      document.querySelectorAll('.task-card').forEach(function (card) {
        cardLabels(card).forEach(function (name) { seen[name] = true; });
      });
      Object.keys(seen).sort().forEach(function (name) { addOption(label, name, name); });

      var member = byId('mu-member');
      member.innerHTML = '';
      addOption(member, 'all', 'Any');
      addOption(member, 'none', 'Unassigned');
      fetchJson('/api/members').catch(function () { return []; }).then(function (roster) {
        (roster || []).forEach(function (user) { addOption(member, user.id, user.username); });
      });

      byId('mu-period').value = 'all';
      byId('mu-due').value = 'all';
      byId('mu-due-day').value = '';
      byId('mu-due-day-field').hidden = true;
    }

    function readFilter() {
      return {
        columns: Array.prototype.filter.call(byId('mu-columns').options, function (o) { return o.selected; })
          .map(function (o) { return o.value; }),
        period: byId('mu-period').value,
        color: byId('mu-color').value,
        label: byId('mu-label').value,
        member: byId('mu-member').value,
        due: byId('mu-due').value,
        dueDay: byId('mu-due-day').value,
      };
    }

    function dueMatches(card, filter) {
      var dueAt = card.dataset.dueAt;
      if (filter.due === 'all') return true;
      if (filter.due === 'has') return !!dueAt;
      if (filter.due === 'none' || filter.due === 'overdue') {
        return BoardChrome.dateMatches(filter.due, dueAt, card.dataset.dueDone === '1');
      }
      if (!dueAt || !filter.dueDay) return false;
      var day = isoDate(parseDueAt(dueAt));
      return filter.due === 'before' ? day <= filter.dueDay : day >= filter.dueDay;
    }

    function cardMatches(card, filter, doneColumns) {
      var list = card.closest('.task-list');
      var columnId = list ? list.dataset.columnId : '';
      if (filter.columns.length && filter.columns.indexOf(columnId) === -1) return false;
      if (filter.period !== 'all' && doneColumns[columnId]) {
        var range = periodToRange(filter.period);
        var day = card.dataset.groupDay || '';
        if (!day || day < range[0] || day > range[1]) return false;
      }
      if (filter.color !== 'all' && card.dataset.colorValue !== filter.color) return false;
      if (filter.label !== 'all' && cardLabels(card).indexOf(filter.label) === -1) return false;
      var members = (card.dataset.memberIds || '').split(',').filter(Boolean);
      if (filter.member === 'none' && members.length) return false;
      if (filter.member !== 'all' && filter.member !== 'none' && members.indexOf(filter.member) === -1) return false;
      return dueMatches(card, filter);
    }

    function findMatches(filter) {
      var names = {};
      var doneColumns = {};
      boardColumns().forEach(function (column) {
        names[column.id] = column.name;
        doneColumns[column.id] = column.done;
      });
      return Array.prototype.filter.call(document.querySelectorAll('.task-card'), function (card) {
        return cardMatches(card, filter, doneColumns);
      }).map(function (card) {
        var list = card.closest('.task-list');
        return {
          id: card.dataset.taskId,
          name: card.dataset.taskName || '',
          color: card.dataset.colorValue || '',
          column: list ? names[list.dataset.columnId] || '' : '',
        };
      });
    }

    function pickedIds() {
      return matches.filter(function (task) { return picked[task.id]; }).map(function (task) { return task.id; });
    }

    function renderCount() {
      var count = matches.length;
      byId('mu-count').textContent = count + (count === 1 ? ' task matches your filter' : ' tasks match your filter');
      byId('mu-next').disabled = !pickedIds().length;
    }

    function renderList() {
      var list = byId('mu-list');
      list.innerHTML = '';
      matches.forEach(function (task) {
        var row = document.createElement('label');
        row.className = 'mu-row';
        var box = document.createElement('input');
        box.type = 'checkbox';
        box.checked = !!picked[task.id];
        box.dataset.taskId = task.id;
        var name = document.createElement('span');
        name.className = 'mu-row-name taskColor-' + task.color;
        name.textContent = task.name;
        var column = document.createElement('span');
        column.className = 'mu-row-column';
        column.textContent = task.column;
        row.appendChild(box);
        row.appendChild(name);
        row.appendChild(column);
        list.appendChild(row);
      });
      renderCount();
    }

    function enterSelect() {
      var filter = readFilter();
      var signature = JSON.stringify(filter);
      if (signature !== lastFilter) {
        lastFilter = signature;
        matches = findMatches(filter);
        picked = {};
        matches.forEach(function (task) { picked[task.id] = true; });
      }
      renderList();
    }

    function fillMoveColumns(selectedId) {
      var select = byId('mu-move-column');
      select.innerHTML = '';
      targetColumns.forEach(function (column) { addOption(select, column.id, column.name); });
      if (selectedId) select.value = selectedId;
    }

    function loadMoveColumns(boardIdValue) {
      if (boardIdValue === boardId()) {
        targetColumns = boardColumns();
        fillMoveColumns(null);
        return;
      }
      fetchJson('/api/boards/' + encodeURIComponent(boardIdValue) + '/columns').then(function (columns) {
        if (byId('mu-move-board').value !== boardIdValue) return;
        targetColumns = columns || [];
        fillMoveColumns(null);
      }).catch(function () { showError('Could not load the columns of that board.'); });
    }

    function chosen(selectId) {
      return Array.prototype.filter.call(byId(selectId).options, function (o) { return o.selected; })
        .map(function (o) { return o.value; });
    }

    function fillRemovableLabels() {
      var held = {};
      pickedIds().forEach(function (id) {
        var card = document.querySelector('.task-card[data-task-id="' + cssEscape(id) + '"]');
        if (card) cardLabels(card).forEach(function (name) { held[name] = true; });
      });
      var keep = chosen('mu-labels-remove');
      var select = byId('mu-labels-remove');
      select.innerHTML = '';
      Object.keys(held).sort().forEach(function (name) {
        addOption(select, name, name);
        select.lastChild.selected = keep.indexOf(name) !== -1;
      });
    }

    function fillMembers() {
      var remove = byId('mu-members-remove');
      var add = byId('mu-members-add');
      remove.innerHTML = '';
      add.innerHTML = '';
      fetchJson('/api/members').catch(function () { return []; }).then(function (roster) {
        (roster || []).forEach(function (user) {
          addOption(remove, user.id, user.username);
          addOption(add, user.id, user.username);
        });
      });
    }

    var subtaskNames = [];
    var subtaskTemplates = [];
    var relationAdds = [];
    var relationSeq = 0;
    var RELATION_WORDS = {
      related: 'Relates to',
      blocked_by: 'Depends on',
      blocks: 'Required by',
      parent: 'Part of',
      child: 'Umbrella of',
    };

    function cardName(id) {
      var card = document.querySelector('.task-card[data-task-id="' + cssEscape(id) + '"]');
      return card ? card.dataset.taskName || '' : '';
    }

    function renderChips(listId, items, label, onRemove) {
      var list = byId(listId);
      list.innerHTML = '';
      items.forEach(function (item, index) {
        var row = document.createElement('li');
        var text = document.createElement('span');
        text.textContent = label(item);
        var remove = document.createElement('button');
        remove.type = 'button';
        remove.className = 'btn btn-sm';
        remove.textContent = 'Remove';
        remove.setAttribute('aria-label', 'Remove ' + label(item));
        remove.addEventListener('click', function () { onRemove(index); });
        row.appendChild(text);
        row.appendChild(remove);
        list.appendChild(row);
      });
    }

    function renderSubtaskRows() {
      renderChips('mu-sub-rows', subtaskNames, function (name) { return name; }, function (index) {
        subtaskNames.splice(index, 1);
        renderSubtaskRows();
      });
    }

    function addSubtaskName() {
      var input = byId('mu-sub-name');
      var name = input.value.trim();
      if (!name) return;
      subtaskNames.push(name);
      input.value = '';
      renderSubtaskRows();
      input.focus();
    }

    function addSubtasksFromTemplate() {
      var select = byId('mu-sub-template');
      var template = subtaskTemplates.filter(function (item) { return item.id === select.value; })[0];
      if (!template) return;
      template.subtasks.forEach(function (name) { subtaskNames.push(name); });
      renderSubtaskRows();
    }

    function fillSubtaskTemplates() {
      var select = byId('mu-sub-template');
      select.innerHTML = '';
      subtaskTemplates = [];
      fetchJson('/api/settings').catch(function () { return {}; }).then(function (settings) {
        subtaskTemplates = settings.subtask_templates || [];
        subtaskTemplates.forEach(function (template) { addOption(select, template.id, template.name); });
        byId('mu-sub-from-template').disabled = !subtaskTemplates.length;
      });
    }

    function showDueFields() {
      var mode = byId('mu-dd-mode').value;
      byId('mu-dd-fields').hidden = mode === 'delete';
      byId('mu-dd-existing-field').hidden = mode !== 'add';
    }

    function fillDueForm() {
      var target = byId('mu-dd-target');
      target.innerHTML = '';
      addOption(target, '', 'None');
      boardColumns().forEach(function (column) { addOption(target, column.id, column.name); });
      byId('mu-dd-mode').value = 'add';
      byId('mu-dd-date').value = '';
      byId('mu-dd-time').value = '';
      byId('mu-dd-existing').value = 'skip';
      showDueFields();
    }

    function relationLabel(link) {
      return (RELATION_WORDS[link.kind] || link.kind) + ' ' + (link.name || cardName(link.target_id) || link.target_id);
    }

    function fillRemovableRelations() {
      var seq = ++relationSeq;
      var select = byId('mu-rel-remove');
      var keep = chosen('mu-rel-remove');
      select.innerHTML = '';
      Promise.all(pickedIds().map(function (id) {
        return fetchJson('/api/tasks/' + encodeURIComponent(id) + '/links').catch(function () { return []; });
      })).then(function (lists) {
        if (seq !== relationSeq) return null;
        var held = {};
        lists.forEach(function (links) {
          links.forEach(function (link) {
            var key = JSON.stringify([link.kind, link.target_id]);
            if (!held[key]) held[key] = { kind: link.kind, target_id: link.target_id, count: 0 };
            held[key].count += 1;
          });
        });
        var entries = Object.keys(held).map(function (key) { return held[key]; });
        return Promise.all(entries.map(function (entry) {
          if (cardName(entry.target_id)) return Promise.resolve();
          return fetchJson('/api/tasks/' + encodeURIComponent(entry.target_id)).then(function (task) {
            entry.name = task.name;
          }).catch(function () {});
        })).then(function () { return entries; });
      }).then(function (entries) {
        if (!entries || seq !== relationSeq) return;
        entries.forEach(function (entry) {
          var key = JSON.stringify({ kind: entry.kind, target_id: entry.target_id });
          addOption(select, key, relationLabel(entry) + ' (' + entry.count + (entry.count === 1 ? ' task)' : ' tasks)'));
          select.lastChild.selected = keep.indexOf(key) !== -1;
        });
      });
    }

    function fillRelationTargets() {
      var picked = pickedIds();
      relationAdds = relationAdds.filter(function (link) { return picked.indexOf(link.target_id) === -1; });
      renderRelationRows();
      var select = byId('mu-rel-target');
      var query = byId('mu-rel-search').value.trim().toLowerCase();
      var chosenIds = pickedIds();
      select.innerHTML = '';
      document.querySelectorAll('.task-card').forEach(function (card) {
        var name = card.dataset.taskName || '';
        if (chosenIds.indexOf(card.dataset.taskId) !== -1) return;
        if (query && name.toLowerCase().indexOf(query) === -1) return;
        addOption(select, card.dataset.taskId, name);
      });
    }

    function renderRelationRows() {
      renderChips('mu-rel-rows', relationAdds, relationLabel, function (index) {
        relationAdds.splice(index, 1);
        renderRelationRows();
      });
    }

    function addRelation() {
      var target = byId('mu-rel-target');
      if (!target.value) return;
      var link = { kind: byId('mu-rel-kind').value, target_id: target.value, name: cardName(target.value) };
      var duplicate = relationAdds.some(function (held) {
        return held.kind === link.kind && held.target_id === link.target_id;
      });
      if (!duplicate) relationAdds.push(link);
      renderRelationRows();
    }

    function resetListForms() {
      subtaskNames = [];
      relationAdds = [];
      byId('mu-sub-name').value = '';
      byId('mu-rel-search').value = '';
      renderSubtaskRows();
      renderRelationRows();
      fillSubtaskTemplates();
      fillDueForm();
    }

    function fillActionForms() {
      var board = byId('mu-move-board');
      board.innerHTML = '';
      addOption(board, boardId(), boardNameFor(boardId()));
      loadMoveColumns(boardId());
      fetchJson('/api/boards').catch(function () { return []; }).then(function (boards) {
        (boards || []).forEach(function (other) {
          if (other.id !== boardId()) addOption(board, other.id, other.name);
        });
      });

      var color = byId('mu-set-color');
      color.innerHTML = '';
      document.querySelectorAll('#board-colors span[data-id]').forEach(function (swatch) {
        addOption(color, swatch.dataset.id, swatch.dataset.label || swatch.dataset.value);
      });
      byId('mu-labels-add').value = '';
      fillMembers();
      resetListForms();
      byId('mu-action').value = 'move';
      showActionForm();
    }

    function showActionForm() {
      var action = byId('mu-action').value;
      document.querySelectorAll('#mass-update-dialog [data-mu-form]').forEach(function (form) {
        form.hidden = form.dataset.muForm !== action;
      });
    }

    function actionRequest() {
      var action = byId('mu-action').value;
      var body = { task_ids: pickedIds(), action: action };
      if (action === 'move') {
        body.column_id = byId('mu-move-column').value;
        body.position = 1000000;
        if (!body.column_id) return 'Choose a column to move the tasks to.';
      } else if (action === 'color') {
        body.color_id = byId('mu-set-color').value;
        if (!body.color_id) return 'Choose a color.';
      } else if (action === 'labels') {
        body.remove_labels = chosen('mu-labels-remove');
        body.add_labels = byId('mu-labels-add').value.split(',').map(function (name) { return name.trim(); })
          .filter(Boolean);
        if (!body.remove_labels.length && !body.add_labels.length) return 'Choose labels to remove or add.';
      } else if (action === 'members') {
        body.remove_member_ids = chosen('mu-members-remove');
        body.add_member_ids = chosen('mu-members-add');
        if (!body.remove_member_ids.length && !body.add_member_ids.length) return 'Choose members to remove or add.';
      } else if (action === 'subtasks') {
        body.add_subtasks = subtaskNames.slice();
        if (!body.add_subtasks.length) return 'Add at least one subtask.';
      } else if (action === 'due_dates') {
        body.due_mode = byId('mu-dd-mode').value;
        if (body.due_mode !== 'delete') {
          var date = byId('mu-dd-date').value;
          if (!date) return 'Choose a date.';
          var time = byId('mu-dd-time').value;
          body.due_at = time ? date + 'T' + time : date;
          if (byId('mu-dd-target').value) body.due_target_column_id = byId('mu-dd-target').value;
          if (body.due_mode === 'add') body.due_existing = byId('mu-dd-existing').value;
        }
      } else if (action === 'relations') {
        body.remove_links = chosen('mu-rel-remove').map(function (key) { return JSON.parse(key); });
        body.add_links = relationAdds.map(function (link) { return { kind: link.kind, target_id: link.target_id }; });
        if (!body.remove_links.length && !body.add_links.length) return 'Choose relations to remove or add.';
      }
      return body;
    }

    function postBulkRequest(body) {
      return api('/api/boards/' + encodeURIComponent(boardId()) + '/tasks/bulk', 'POST', body).then(function (res) {
        return res.text().then(function (text) {
          var data = null;
          try { data = JSON.parse(text); } catch (e) { data = null; }
          return { ok: res.ok, data: data, message: (data && data.message) || text };
        });
      });
    }

    function renderPreview(tasks) {
      var body = byId('mu-preview-body');
      body.innerHTML = '';
      tasks.forEach(function (task) {
        var row = document.createElement('tr');
        [task.name, task.before, task.after].forEach(function (text) {
          var cell = document.createElement('td');
          cell.textContent = text === null ? '(none)' : text;
          row.appendChild(cell);
        });
        body.appendChild(row);
      });
      var deleting = !!request && request.action === 'delete';
      var dueDates = !!request && request.action === 'due_dates';
      var count = dueDates ? tasks.filter(function (task) { return task.before !== task.after; }).length : tasks.length;
      var skipped = tasks.length - count;
      var noun = count === 1 ? ' task' : ' tasks';
      var text = count + noun + (deleting ? ' will be deleted' : ' will be updated');
      if (skipped) text += ', ' + skipped + (skipped === 1 ? ' task is' : ' tasks are') + ' skipped';
      byId('mu-preview-count').textContent = text;
    }

    function enterPreview() {
      var body = actionRequest();
      if (typeof body === 'string') {
        show(step - 1);
        showError(body);
        return;
      }
      request = body;
      var seq = ++previewSeq;
      renderPreview([]);
      byId('mu-apply').disabled = true;
      var dry = { dry_run: true };
      Object.keys(body).forEach(function (key) { dry[key] = body[key]; });
      postBulkRequest(dry).then(function (res) {
        if (seq !== previewSeq || STEPS[step] !== 'preview') return;
        if (!res.ok) {
          show(step - 1);
          showError(res.message || 'The update was refused, so nothing would change.');
          return;
        }
        renderPreview(res.data.tasks || []);
        byId('mu-apply').disabled = false;
      }).catch(function () {
        if (seq !== previewSeq || STEPS[step] !== 'preview') return;
        show(step - 1);
        showError('Could not preview the update.');
      });
    }

    function apply() {
      if (!request || applying) return;
      var count = request.task_ids.length;
      var deleting = request.action === 'delete';
      var message = count + (count === 1 ? ' task' : ' tasks') + (deleting ? ' will be deleted. Do you want to continue?' : ' will be updated. Do you want to continue?');
      var verb = deleting ? 'Delete tasks' : 'Apply updates';
      showConfirmDialog(verb, message, verb, function () {
        applying = true;
        postBulkRequest(request).then(function (res) {
          applying = false;
          if (res.ok) {
            window.location.reload();
            return;
          }
          show(2);
          showError(res.message || 'The update was refused, so nothing changed.');
        }).catch(function () {
          applying = false;
          showError('Could not apply the update.');
        });
      });
    }

    function show(index) {
      step = index;
      showError('');
      STEPS.forEach(function (name, i) { stepSection(name).hidden = i !== step; });
      var name = STEPS[step];
      byId('mu-prev').hidden = step === 0;
      byId('mu-next').hidden = name === 'preview';
      byId('mu-apply').hidden = name !== 'preview';
      byId('mu-next').disabled = false;
      if (name === 'select') enterSelect();
      else if (name === 'action') {
        fillRemovableLabels();
        fillRemovableRelations();
        fillRelationTargets();
      }
      else if (name === 'preview') enterPreview();
    }

    var wired = false;
    function wire() {
      if (wired) return;
      wired = true;
      byId('mu-next').addEventListener('click', function () { show(step + 1); });
      byId('mu-prev').addEventListener('click', function () { show(step - 1); });
      byId('mu-apply').addEventListener('click', apply);
      byId('mu-action').addEventListener('change', showActionForm);
      byId('mu-sub-add').addEventListener('click', addSubtaskName);
      byId('mu-sub-name').addEventListener('keydown', function (e) {
        if (e.key === 'Enter') { e.preventDefault(); addSubtaskName(); }
      });
      byId('mu-sub-from-template').addEventListener('click', addSubtasksFromTemplate);
      byId('mu-dd-mode').addEventListener('change', showDueFields);
      byId('mu-rel-search').addEventListener('input', fillRelationTargets);
      byId('mu-rel-add').addEventListener('click', addRelation);
      byId('mu-move-board').addEventListener('change', function () {
        loadMoveColumns(byId('mu-move-board').value);
      });
      byId('mu-due').addEventListener('change', function () {
        var value = byId('mu-due').value;
        byId('mu-due-day-field').hidden = value !== 'before' && value !== 'after';
      });
      byId('mu-select-all').addEventListener('click', function () {
        matches.forEach(function (task) { picked[task.id] = true; });
        renderList();
      });
      byId('mu-clear-all').addEventListener('click', function () {
        picked = {};
        renderList();
      });
      byId('mu-list').addEventListener('change', function (e) {
        var box = e.target.closest('input[type="checkbox"]');
        if (!box) return;
        if (box.checked) picked[box.dataset.taskId] = true;
        else delete picked[box.dataset.taskId];
        renderCount();
      });
    }

    function open() {
      wire();
      fillFilters();
      fillActionForms();
      matches = [];
      picked = {};
      lastFilter = '';
      show(0);
      byId('mass-update-dialog').hidden = false;
    }

    return { open: open };
  })();

  // Exposed for data-call handlers and debugging.
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
  window.TimelinePage = TimelinePage;
  window.BoardHistoryPage = BoardHistoryPage;
  window.CalendarPage = CalendarPage;
  window.MonteCarloPage = MonteCarloPage;
  window.TimeEstimatePage = TimeEstimatePage;
  window.TimeInColumnPage = TimeInColumnPage;
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
        openTimeLogView();
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
