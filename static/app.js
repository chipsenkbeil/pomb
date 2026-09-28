/* ChipFlow frontend glue: drag-and-drop, task modal, and the global timer.
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

  // ---------- drag and drop ----------

  function initSortable() {
    if (typeof Sortable === 'undefined') return;
    document.querySelectorAll('.task-list').forEach(function (list) {
      if (list._sortable) return;
      list._sortable = new Sortable(list, {
        group: 'tasks',
        animation: 150,
        draggable: '.task-card',
        ghostClass: 'drag-ghost',
        onStart: function () { dragging = true; },
        onEnd: function (evt) {
          window.setTimeout(function () { dragging = false; }, 80);
          var card = evt.item;
          var toList = evt.to;
          var cards = Array.prototype.slice.call(toList.querySelectorAll('.task-card'));
          var position = cards.indexOf(card);
          fetch('/api/tasks/' + encodeURIComponent(card.dataset.taskId) + '/move', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              column_id: toList.dataset.columnId,
              swimlane_id: toList.dataset.swimlaneId || null,
              position: position,
            }),
          }).then(function (res) {
            if (!res.ok) window.location.reload(); // resync the board on failure
          }).catch(function () { window.location.reload(); });
        },
      });
    });
  }
  // Note: htmx appends new cards inside the existing .task-list containers,
  // which already have Sortable attached, so no re-init is needed.

  // ---------- task modal ----------

  function modalTaskId() {
    var modal = document.querySelector('#modal-root .modal');
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
    document.removeEventListener('keydown', escHandler);
    document.getElementById('modal-root').innerHTML = '';
    if (modalDirty) window.location.reload();
  }

  function escHandler(e) {
    if (e.key === 'Escape') {
      if (!document.getElementById('why-stop-menu').hidden) TimerUI.closeWhyMenu();
      else closeModal();
    }
  }

  function wireModal() {
    var overlay = document.getElementById('modal-overlay');
    if (!overlay) return;
    overlay.addEventListener('click', function (e) {
      if (e.target === overlay) closeModal();
    });
    document.addEventListener('keydown', escHandler);

    // Size picker: PATCH the size, highlight the choice, mark dirty.
    overlay.querySelectorAll('.size-picker button').forEach(function (btn) {
      btn.addEventListener('click', function () {
        var id = modalTaskId();
        fetch('/api/tasks/' + encodeURIComponent(id), {
          method: 'PATCH',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ size: parseInt(btn.dataset.size, 10) }),
        }).then(function (res) {
          if (res.ok) {
            overlay.querySelectorAll('.size-picker button').forEach(function (b) {
              b.classList.remove('selected');
            });
            btn.classList.add('selected');
            modalDirty = true;
          }
        });
      });
    });

    // Manual "log time" form: POST, then swap in the refreshed entries list.
    var form = document.getElementById('log-time-form');
    form.addEventListener('submit', function (e) {
      e.preventDefault();
      var id = modalTaskId();
      fetch('/api/tasks/' + encodeURIComponent(id) + '/time', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          minutes: parseInt(form.minutes.value, 10),
          note: form.note.value,
        }),
      })
        .then(function (res) { return res.ok ? res.text() : Promise.reject(res.status); })
        .then(function (html) {
          document.getElementById('time-entries').innerHTML = html;
          form.note.value = '';
          modalDirty = true;
        })
        .catch(function () { /* keep the form open on failure */ });
    });
  }

  function saveModalTask() {
    var id = modalTaskId();
    var name = document.getElementById('modal-name').value.trim();
    var description = document.getElementById('modal-description').value;
    if (!name || !id) return;
    fetch('/api/tasks/' + encodeURIComponent(id), {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: name, description: description }),
    }).then(function (res) { if (res.ok) modalDirty = true; });
  }

  function deleteModalTask() {
    var id = modalTaskId();
    if (!id || !window.confirm('Delete this task and its time entries?')) return;
    fetch('/api/tasks/' + encodeURIComponent(id), { method: 'DELETE' })
      .then(function (res) {
        if (res.ok) { modalDirty = true; closeModal(); }
      });
  }

  function toggleTimerMenu(e) {
    e.stopPropagation();
    var menu = document.getElementById('timer-menu');
    menu.hidden = !menu.hidden;
  }

  function scrollToTimeLog() {
    var menu = document.getElementById('timer-menu');
    if (menu) menu.hidden = true;
    var heading = document.getElementById('time-log-heading');
    if (heading) heading.scrollIntoView({ behavior: 'smooth', block: 'start' });
  }

  // ---------- toast ----------

  var toastTimer = null;
  function toast(message) {
    var el = document.getElementById('toast');
    if (!el) return;
    el.innerHTML = message;
    el.hidden = false;
    if (toastTimer) window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(function () { el.hidden = true; }, 4000);
  }

  // ---------- global timer ----------

  var MODE_COLORS = {
    pomodoro: '#e57373',
    stopwatch: '#64b5f6',
    short_break: '#81c784',
    long_break: '#4db6ac',
  };

  var TimerUI = {
    settings: null,
    state: { active: false },
    idleMode: 'pomodoro',
    tickHandle: null,
    endNotified: false,
    popupOpen: false,

    init: function () {
      var pill = document.getElementById('timer-pill');
      if (!pill) return; // not on a board page
      var self = this;
      Promise.all([
        fetch('/api/settings').then(function (r) { return r.json(); }),
        fetch('/api/timer/status').then(function (r) { return r.json(); }),
      ]).then(function (results) {
        self.settings = results[0];
        self.setState(results[1]);
        self.tickHandle = window.setInterval(function () { self.tick(); }, 1000);
        self.refreshToday();
      }).catch(function () { /* timer stays hidden when offline */ });
      pill.addEventListener('click', function () { self.togglePopup(); });
      document.addEventListener('click', function (e) {
        var popup = document.getElementById('timer-popup');
        var why = document.getElementById('why-stop-menu');
        if (!popup.hidden && !popup.contains(e.target) && !pill.contains(e.target)) {
          self.closePopup();
        }
        if (!why.hidden && !why.contains(e.target) && !e.target.closest('#timer-popup-stop')) {
          self.closeWhyMenu();
        }
      });
    },

    setState: function (state) {
      this.state = state;
      this.endNotified = false;
      this.render();
    },

    elapsedSecs: function () {
      if (!this.state.active || !this.state.started_at) return 0;
      return Math.max(0, Math.floor(Date.now() / 1000) - this.state.started_at);
    },

    fmt: function (totalSecs) {
      var m = Math.floor(totalSecs / 60);
      var s = totalSecs % 60;
      return String(m).padStart(2, '0') + ':' + String(s).padStart(2, '0');
    },

    tick: function () {
      if (!this.state.active) return;
      var elapsed = this.elapsedSecs();
      var dur = this.state.duration_secs;
      // Natural end: ding + notification once, then count overtime.
      if (dur != null && elapsed >= dur && !this.endNotified) {
        this.endNotified = true;
        this.onNaturalEnd();
      }
      this.renderClock();
    },

    onNaturalEnd: function () {
      var title = this.state.mode_title || 'Timer';
      if (this.settings && this.settings.ding_enabled) this.ding();
      this.notify(title + ' finished', this.endBody());
      // After a pomodoro, offer a break in the popup.
      var breaks = document.getElementById('timer-popup-breaks');
      if (breaks && this.state.mode === 'pomodoro') breaks.hidden = false;
    },

    endBody: function () {
      switch (this.state.mode) {
        case 'pomodoro': return 'Time for a break.';
        case 'short_break':
        case 'long_break': return 'Break over — back to it.';
        default: return '';
      }
    },

    ding: function () {
      try {
        var Ctx = window.AudioContext || window.webkitAudioContext;
        var ctx = new Ctx();
        var t = ctx.currentTime;
        [880, 1174.66].forEach(function (freq, i) {
          var osc = ctx.createOscillator();
          var gain = ctx.createGain();
          osc.type = 'sine';
          osc.frequency.value = freq;
          var start = t + i * 0.4;
          gain.gain.setValueAtTime(0.0001, start);
          gain.gain.exponentialRampToValueAtTime(0.4, start + 0.03);
          gain.gain.exponentialRampToValueAtTime(0.0001, start + 0.7);
          osc.connect(gain);
          gain.connect(ctx.destination);
          osc.start(start);
          osc.stop(start + 0.8);
        });
      } catch (e) { /* audio unavailable — stay silent */ }
    },

    notify: function (title, body) {
      if (!this.settings || !this.settings.notifications_enabled) return;
      try {
        if (!('Notification' in window)) return;
        if (Notification.permission === 'granted') {
          new Notification('ChipFlow', { body: body ? title + ' — ' + body : title });
        } else if (Notification.permission !== 'denied') {
          Notification.requestPermission();
        }
      } catch (e) { /* notifications unavailable */ }
    },

    render: function () {
      var pill = document.getElementById('timer-pill');
      if (!pill) return;
      if (!this.state.active) {
        this.renderIdle();
        return;
      }
      pill.hidden = false;
      var dot = document.getElementById('timer-pill-dot');
      dot.style.background = MODE_COLORS[this.state.mode] || '#e57373';
      dot.style.color = '';
      dot.textContent = '';
      document.getElementById('timer-popup-title').textContent = this.state.mode_title || 'Timer';
      var label = document.getElementById('timer-popup-label');
      label.textContent = this.state.mode === 'stopwatch' ? 'Session time'
        : this.state.mode === 'pomodoro' ? 'Time until break'
        : 'Time remaining';
      document.getElementById('timer-popup-stop').hidden = false;
      document.getElementById('timer-popup-start').hidden = true;
      this.renderTaskRow();
      this.renderModeTab();
      this.renderClock();
    },

    // Idle pill + popup, mirroring KanbanFlow: pomodoro idle shows
    // "▶ 25:00 ▾" (v3-03212); stopwatch idle shows red ■ + "00:00" + ⌄ (v4-00142).
    renderIdle: function () {
      var pill = document.getElementById('timer-pill');
      pill.hidden = false;
      document.title = document.title.replace(/^\([\d:+]+\) /, '');
      var dot = document.getElementById('timer-pill-dot');
      var time = document.getElementById('timer-pill-time');
      dot.style.background = 'transparent';
      if (this.idleMode === 'stopwatch') {
        dot.style.color = '#e57373';
        dot.textContent = '■';
        time.textContent = '00:00 ▾';
        document.getElementById('timer-popup-title').textContent = 'Stopwatch';
        document.getElementById('timer-popup-label').textContent = 'Session time';
        document.getElementById('timer-popup-time').textContent = '00:00';
      } else {
        var mins = (this.settings && this.settings.pomodoro_minutes) || 25;
        dot.style.color = '#81c784';
        dot.textContent = '▶';
        time.textContent = this.fmt(mins * 60) + ' ▾';
        document.getElementById('timer-popup-title').textContent = 'Pomodoro';
        document.getElementById('timer-popup-label').textContent = 'Time until break';
        document.getElementById('timer-popup-time').textContent = this.fmt(mins * 60);
      }
      document.getElementById('timer-popup-stop').hidden = true;
      document.getElementById('timer-popup-start').hidden = false;
      this.renderTaskRow();
      this.renderModeTab();
    },

    // Bottom-nav first tab names the OTHER mode (v4-00002, v4-00037).
    renderModeTab: function () {
      var btn = document.getElementById('timer-foot-mode');
      if (!btn) return;
      var shown = this.state.active ? this.state.mode : this.idleMode;
      var other = shown === 'stopwatch' ? 'pomodoro' : 'stopwatch';
      btn.dataset.mode = other;
      btn.title = other === 'stopwatch' ? 'Stopwatch' : 'Pomodoro';
      var label = btn.querySelector('span');
      if (label) label.textContent = other === 'stopwatch' ? 'Stopwatch' : 'Pomodoro';
    },

    switchModeTab: function () {
      if (this.state.active) {
        toast('Stop the current timer first.');
        return;
      }
      var btn = document.getElementById('timer-foot-mode');
      this.idleMode = (btn && btn.dataset.mode) || 'stopwatch';
      this.render();
    },

    // Task row: "Change task" normally; "Select open task" when a different
    // task's modal is open (v1-02073; binding behavior inferred).
    renderTaskRow: function () {
      var btn = document.getElementById('timer-popup-task-btn');
      var name = document.getElementById('timer-popup-task-name');
      if (name) name.textContent = this.state.task_name || 'No task';
      if (!btn) return;
      var modalId = modalTaskId();
      if (modalId && modalId !== this.state.task_id) {
        btn.textContent = 'Select open task';
        btn.onclick = function () { TimerUI.selectOpenTask(); };
      } else {
        btn.textContent = 'Change task';
        btn.onclick = function () { TimerUI.changeTask(); };
      }
    },

    selectOpenTask: function () {
      var id = modalTaskId();
      if (!id) return;
      var self = this;
      fetch('/api/timer/retarget', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ task_id: id }),
      })
        .then(function (res) { return res.ok ? res.json() : Promise.reject(res.status); })
        .then(function (status) { self.setState(status); })
        .catch(function () { toast('Could not select task.'); });
    },

    renderClock: function () {
      if (!this.state.active) return;
      var elapsed = this.elapsedSecs();
      var dur = this.state.duration_secs;
      var text, title;
      if (dur != null && elapsed >= dur) {
        var over = elapsed - dur;
        text = '+' + this.fmt(over);
        title = '(' + text + ') ChipFlow';
      } else if (dur != null) {
        text = this.fmt(dur - elapsed);
        title = '(' + text + ') ChipFlow';
      } else {
        text = this.fmt(elapsed);
        title = '(' + text + ') ChipFlow';
      }
      document.getElementById('timer-pill-time').textContent = text;
      document.getElementById('timer-popup-time').textContent = text;
      document.title = title;
    },

    // ----- popup -----

    togglePopup: function () {
      if (this.popupOpen) this.closePopup();
      else this.openPopup();
    },

    openPopup: function () {
      this.popupOpen = true;
      document.getElementById('timer-popup').hidden = false;
      this.refreshToday();
    },

    closePopup: function () {
      this.popupOpen = false;
      document.getElementById('timer-popup').hidden = true;
    },

    openLog: function () {
      this.openPopup();
    },

    refreshToday: function () {
      var self = this;
      fetch('/api/timer/today')
        .then(function (r) { return r.json(); })
        .then(function (entries) {
          var list = document.getElementById('timer-today-list');
          if (!list) return;
          if (!entries.length) {
            list.innerHTML = '<p class="empty-note">Nothing logged today yet.</p>';
            return;
          }
          list.innerHTML = entries.map(function (e) {
            var flag = e.interrupted ? ' &#9888;' : '';
            var reason = e.interrupt_reason
              ? ' <span class="today-reason">(' + escapeHtml(e.interrupt_reason) + ')</span>'
              : '';
            return '<div class="today-entry"><span class="today-dot" style="background:' +
              (MODE_COLORS[e.kind] || '#ccc') + '"></span><span class="today-task">' +
              escapeHtml(e.task_name) + '</span><span class="today-meta">' +
              escapeHtml(e.started_display) + ' &mdash; ' + e.minutes + 'm ' +
              escapeHtml(e.kind_label) + flag + '</span>' + reason + '</div>';
          }).join('');
        })
        .catch(function () { /* leave the list as-is */ });
    },

    // ----- starting -----

    start: function (mode, taskId) {
      var self = this;
      fetch('/api/timer/start', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ task_id: taskId || null, mode: mode }),
      })
        .then(function (res) { return res.ok ? res.json() : Promise.reject(res.status); })
        .then(function (status) {
          self.setState(status);
          document.getElementById('timer-popup-breaks').hidden = true;
          if (!self.popupOpen) self.openPopup();
        })
        .catch(function () { toast('Could not start the timer.'); });
    },

    startForTask: function (mode) {
      var menu = document.getElementById('timer-menu');
      if (menu) menu.hidden = true;
      this.start(mode, modalTaskId());
    },

    // Start button in the idle popup: begins the displayed idle mode.
    startIdle: function () {
      this.start(this.idleMode, this.state.task_id || null);
    },

    startBreak: function (mode) {
      this.start(mode, null);
    },

    changeTask: function () {
      var self = this;
      var cards = Array.prototype.slice.call(document.querySelectorAll('.task-card'));
      if (!cards.length) { toast('No tasks on this board.'); return; }
      var lines = cards.map(function (card, i) {
        var name = card.querySelector('.task-name');
        return (i + 1) + '. ' + (name ? name.textContent.trim() : card.dataset.taskId);
      });
      var raw = window.prompt('Move the timer to which task?\n' + lines.join('\n'));
      if (!raw) return;
      var index = parseInt(raw, 10) - 1;
      var card = cards[index];
      if (!card) { toast('No such task.'); return; }
      fetch('/api/timer/retarget', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ task_id: card.dataset.taskId }),
      })
        .then(function (res) { return res.ok ? res.json() : Promise.reject(res.status); })
        .then(function (status) { self.setState(status); })
        .catch(function () { toast('Could not change task.'); });
    },

    addTime: function () {
      // Opens the "Add time manually" dialog (v3-00001), pre-filled with the
      // timer's task when one is selected.
      ManualTime.open(this.state.task_id, this.state.task_name || '');
    },

    // ----- stopping -----

    stopClicked: function () {
      if (!this.state.active) return;
      var elapsed = this.elapsedSecs();
      var dur = this.state.duration_secs;
      var completed = dur != null && elapsed >= dur;
      // Pomodoro stopped early -> "Why did you stop?". Everything else
      // (breaks, stopwatch, completed pomodoro) stops directly.
      if (this.state.mode === 'pomodoro' && !completed) {
        this.openWhyMenu();
      } else {
        this.confirmStop(true, null);
      }
    },

    openWhyMenu: function () {
      var self = this;
      var menu = document.getElementById('why-stop-menu');
      var box = document.getElementById('why-stop-reasons');
      var reasons = (this.settings && this.settings.interrupt_reasons) || [];
      box.innerHTML = '';
      reasons.forEach(function (reason) {
        var btn = document.createElement('button');
        btn.type = 'button';
        btn.textContent = reason;
        btn.addEventListener('click', function () { self.confirmStop(false, reason); });
        box.appendChild(btn);
      });
      var add = document.createElement('button');
      add.type = 'button';
      add.className = 'why-add';
      add.textContent = 'Add new reason...';
      add.addEventListener('click', function () {
        var custom = window.prompt('Reason for stopping:');
        if (custom && custom.trim()) self.confirmStop(false, custom.trim());
      });
      box.appendChild(add);
      menu.hidden = false;
    },

    closeWhyMenu: function () {
      document.getElementById('why-stop-menu').hidden = true;
    },

    confirmStop: function (completed, reason) {
      var self = this;
      this.closeWhyMenu();
      fetch('/api/timer/stop', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ completed: completed, reason: reason || null }),
      })
        .then(function (res) { return res.ok ? res.json() : Promise.reject(res.status); })
        .then(function (result) {
          return fetch('/api/timer/status')
            .then(function (r) { return r.json(); })
            .then(function (status) { return { result: result, status: status }; });
        })
        .then(function (both) {
          var wasMode = self.state.mode;
          self.setState(both.status);
          self.refreshToday();
          if (both.result.discarded) {
            toast('Session discarded<br><span class="toast-sub">Session lasted less than 20 seconds</span>');
          } else if (both.result.completed && wasMode === 'pomodoro') {
            var breaks = document.getElementById('timer-popup-breaks');
            if (breaks) breaks.hidden = false;
            if (!self.popupOpen) self.openPopup();
            toast('Pomodoro complete — time for a break.');
          }
          // Refresh the modal so pomodori/interruption counts update.
          if (modalTaskId()) refreshModal();
          else modalDirty = true;
        })
        .catch(function () { toast('Could not stop the timer.'); });
    },
  };

  // ---------- card context menu ----------
  // Timer submenu: "Start timer" / "Select in timer" (v1-00306, v1-00354).

  var cardMenuTaskId = null;

  function showCardMenu(e) {
    var card = e.target.closest('.task-card');
    if (!card) return;
    e.preventDefault();
    cardMenuTaskId = card.dataset.taskId;
    var menu = document.getElementById('card-menu');
    menu.hidden = false;
    var x = Math.min(e.clientX, window.innerWidth - 180);
    var y = Math.min(e.clientY, window.innerHeight - 120);
    menu.style.left = x + 'px';
    menu.style.top = y + 'px';
  }

  function hideCardMenu() {
    var menu = document.getElementById('card-menu');
    if (menu) menu.hidden = true;
    cardMenuTaskId = null;
  }

  function cardMenuStartTimer() {
    var id = cardMenuTaskId;
    hideCardMenu();
    if (!id) return;
    TimerUI.start(TimerUI.idleMode, id);
  }

  function cardMenuSelectInTimer() {
    var id = cardMenuTaskId;
    hideCardMenu();
    if (!id) return;
    if (!TimerUI.state.active) {
      toast('No timer is running.');
      return;
    }
    fetch('/api/timer/retarget', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ task_id: id }),
    })
      .then(function (res) { return res.ok ? res.json() : Promise.reject(res.status); })
      .then(function (status) { TimerUI.setState(status); })
      .catch(function () { toast('Could not select task.'); });
  }

  document.addEventListener('contextmenu', showCardMenu);
  document.addEventListener('click', function (e) {
    var menu = document.getElementById('card-menu');
    if (menu && !menu.hidden && !menu.contains(e.target)) hideCardMenu();
  });
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape') hideCardMenu();
  });

  // ---------- shared time-dialog helpers ----------

  var taskNameCache = null; // [{id, name}]

  function fetchTaskNames() {
    if (taskNameCache) return Promise.resolve(taskNameCache);
    return fetch('/api/tasks')
      .then(function (res) { return res.ok ? res.json() : []; })
      .then(function (list) { taskNameCache = list; return list; })
      .catch(function () { return []; });
  }

  function fillTaskDatalist() {
    fetchTaskNames().then(function (list) {
      var dl = document.getElementById('mt-task-list');
      dl.innerHTML = '';
      list.forEach(function (t) {
        var opt = document.createElement('option');
        opt.value = t.name;
        dl.appendChild(opt);
      });
    });
  }

  function resolveTaskId(name, fallbackId) {
    if (!taskNameCache) return fallbackId;
    var lower = (name || '').trim().toLowerCase();
    for (var i = 0; i < taskNameCache.length; i++) {
      if (taskNameCache[i].name.toLowerCase() === lower) return taskNameCache[i].id;
    }
    return fallbackId;
  }

  /// "0h" for zero, else "Xh Ym" / "Xh" / "Ym" (v3-00001).
  function formatDuration(minutes) {
    if (minutes <= 0) return '0h';
    var h = Math.floor(minutes / 60);
    var m = minutes % 60;
    if (h > 0 && m > 0) return h + 'h ' + m + 'm';
    if (h > 0) return h + 'h';
    return m + 'm';
  }

  function minutesBetween(dateStr, fromStr, toStr) {
    if (!dateStr || !fromStr || !toStr) return 0;
    var d = new Date(dateStr + 'T' + fromStr + ':00');
    var e = new Date(dateStr + 'T' + toStr + ':00');
    if (isNaN(d.getTime()) || isNaN(e.getTime())) return 0;
    return Math.max(0, Math.round((e - d) / 60000));
  }

  function todayStr() {
    var d = new Date();
    return d.getFullYear() + '-' + String(d.getMonth() + 1).padStart(2, '0') + '-' + String(d.getDate()).padStart(2, '0');
  }

  // Calendar popup: month grid, Sun-Sat, selected day blue (v3-00001).
  function renderCalendar(popupId, year, month, selectedStr, onPick) {
    var popup = document.getElementById(popupId);
    var names = ['January','February','March','April','May','June','July','August','September','October','November','December'];
    var html = '<div class="mt-cal-head">'
      + '<button type="button" data-cal-nav="-1" aria-label="Previous month">&lt;</button>'
      + '<span>' + names[month] + ' ' + year + '</span>'
      + '<button type="button" data-cal-nav="1" aria-label="Next month">&gt;</button>'
      + '</div><div class="mt-cal-grid">';
    ['Su','Mo','Tu','We','Th','Fr','Sa'].forEach(function (d) { html += '<div class="mt-cal-dow">' + d + '</div>'; });
    var first = new Date(year, month, 1).getDay();
    var days = new Date(year, month + 1, 0).getDate();
    for (var i = 0; i < first; i++) html += '<div></div>';
    for (var day = 1; day <= days; day++) {
      var ds = year + '-' + String(month + 1).padStart(2, '0') + '-' + String(day).padStart(2, '0');
      var cls = 'mt-cal-day' + (ds === selectedStr ? ' selected' : '');
      html += '<button type="button" class="' + cls + '" data-cal-day="' + ds + '">' + day + '</button>';
    }
    html += '</div>';
    popup.innerHTML = html;
    popup.hidden = false;
    popup.querySelectorAll('[data-cal-nav]').forEach(function (btn) {
      btn.onclick = function (ev) {
        ev.stopPropagation();
        var ny = year, nm = month + parseInt(btn.getAttribute('data-cal-nav'), 10);
        if (nm < 0) { nm = 11; ny--; } else if (nm > 11) { nm = 0; ny++; }
        renderCalendar(popupId, ny, nm, selectedStr, onPick);
      };
    });
    popup.querySelectorAll('[data-cal-day]').forEach(function (btn) {
      btn.onclick = function (ev) {
        ev.stopPropagation();
        onPick(btn.getAttribute('data-cal-day'));
        popup.hidden = true;
      };
    });
  }

  function positionCalendar(popupId, anchorId) {
    var popup = document.getElementById(popupId);
    var anchor = document.getElementById(anchorId);
    var r = anchor.getBoundingClientRect();
    popup.style.left = Math.min(r.left, window.innerWidth - 260) + 'px';
    popup.style.top = (r.bottom + 6) + 'px';
  }

  // ---------- Add time manually (v3-00001) ----------

  var ManualTime = {
    taskId: null,
    calYear: 0,
    calMonth: 0,

    open: function (taskId, taskName) {
      this.taskId = taskId || null;
      fillTaskDatalist();
      document.getElementById('mt-task').value = taskName || '';
      var today = todayStr();
      document.getElementById('mt-date').value = today;
      var now = new Date();
      var hh = String(now.getHours()).padStart(2, '0');
      var mm = String(now.getMinutes()).padStart(2, '0');
      document.getElementById('mt-from').value = hh + ':' + mm;
      document.getElementById('mt-to').value = hh + ':' + mm;
      document.getElementById('mt-comment').value = '';
      document.getElementById('mt-comment').hidden = true;
      document.getElementById('mt-comment-toggle').textContent = '+ Add comment';
      this.updateDuration();
      document.getElementById('mt-cal-popup').hidden = true;
      document.getElementById('manual-time-overlay').hidden = false;
      document.getElementById('mt-task').focus();
    },

    close: function () {
      document.getElementById('manual-time-overlay').hidden = true;
      document.getElementById('mt-cal-popup').hidden = true;
    },

    updateDuration: function () {
      var mins = minutesBetween(
        document.getElementById('mt-date').value,
        document.getElementById('mt-from').value,
        document.getElementById('mt-to').value
      );
      document.getElementById('mt-duration').textContent = formatDuration(mins);
    },

    openCalendar: function () {
      var cur = document.getElementById('mt-date').value || todayStr();
      var parts = cur.split('-');
      this.calYear = parseInt(parts[0], 10);
      this.calMonth = parseInt(parts[1], 10) - 1;
      var self = this;
      renderCalendar('mt-cal-popup', this.calYear, this.calMonth, cur, function (ds) {
        document.getElementById('mt-date').value = ds;
        self.updateDuration();
      });
      positionCalendar('mt-cal-popup', 'mt-cal-btn');
    },

    toggleComment: function () {
      var ta = document.getElementById('mt-comment');
      ta.hidden = !ta.hidden;
      document.getElementById('mt-comment-toggle').textContent = ta.hidden ? '+ Add comment' : '- Hide comment';
    },

    submit: function () {
      var date = document.getElementById('mt-date').value;
      var from = document.getElementById('mt-from').value;
      var to = document.getElementById('mt-to').value;
      var taskName = document.getElementById('mt-task').value;
      var taskId = resolveTaskId(taskName, this.taskId);
      if (!taskId) {
        this.showError('Please pick a task');
        return;
      }
      var btn = document.getElementById('mt-add');
      btn.disabled = true;
      btn.textContent = 'Adding…';
      var self = this;
      fetch('/api/time/manual', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          task_id: taskId,
          date: date,
          from: from,
          to: to,
          note: document.getElementById('mt-comment').value,
        }),
      })
        .then(function (res) {
          return res.json().then(function (body) { return { ok: res.ok, body: body }; });
        })
        .then(function (result) {
          btn.disabled = false;
          btn.textContent = 'Add';
          if (result.ok) {
            self.close();
            toast('Time added.');
            refreshModalTimeLog();
          } else {
            // Exact KanbanFlow message; dialog state is preserved (v3-00202).
            self.showError(result.body.error || 'Could not add time.');
          }
        })
        .catch(function () {
          btn.disabled = false;
          btn.textContent = 'Add';
          self.showError('Could not add time.');
        });
    },

    showError: function (msg) {
      document.getElementById('mt-error-msg').textContent = msg;
      document.getElementById('mt-error-overlay').hidden = false;
    },

    closeError: function () {
      document.getElementById('mt-error-overlay').hidden = true;
    },
  };

  // ---------- Edit time entry (v3-01841) ----------

  var EditEntry = {
    entryId: null,
    taskId: null,

    open: function (entryId, entry) {
      this.entryId = entryId;
      this.taskId = entry.taskId;
      fillTaskDatalist();
      document.getElementById('ee-task').value = entry.taskName || '';
      // entry.startedAt is "YYYY-MM-DDTHH:MM"; split into date + time.
      var parts = (entry.startedAt || '').split('T');
      document.getElementById('ee-date').value = parts[0] || todayStr();
      var from = (parts[1] || '09:00').slice(0, 5);
      document.getElementById('ee-from').value = from;
      // To = from + minutes.
      var d = new Date((parts[0] || todayStr()) + 'T' + from + ':00');
      d = new Date(d.getTime() + (entry.minutes || 0) * 60000);
      document.getElementById('ee-to').value =
        String(d.getHours()).padStart(2, '0') + ':' + String(d.getMinutes()).padStart(2, '0');
      document.getElementById('ee-comment').value = entry.note || '';
      document.getElementById('ee-comment').hidden = !entry.note;
      document.getElementById('ee-comment-toggle').textContent = entry.note ? '- Hide comment' : '+ Add comment';
      this.updateDuration();
      document.getElementById('ee-cal-popup').hidden = true;
      document.getElementById('edit-entry-overlay').hidden = false;
    },

    close: function () {
      document.getElementById('edit-entry-overlay').hidden = true;
      document.getElementById('ee-cal-popup').hidden = true;
    },

    updateDuration: function () {
      var mins = minutesBetween(
        document.getElementById('ee-date').value,
        document.getElementById('ee-from').value,
        document.getElementById('ee-to').value
      );
      document.getElementById('ee-duration').textContent = formatDuration(mins);
    },

    openCalendar: function () {
      var cur = document.getElementById('ee-date').value || todayStr();
      var parts = cur.split('-');
      var self = this;
      renderCalendar('ee-cal-popup', parseInt(parts[0], 10), parseInt(parts[1], 10) - 1, cur, function (ds) {
        document.getElementById('ee-date').value = ds;
        self.updateDuration();
      });
      positionCalendar('ee-cal-popup', 'ee-cal-btn');
    },

    toggleComment: function () {
      var ta = document.getElementById('ee-comment');
      ta.hidden = !ta.hidden;
      document.getElementById('ee-comment-toggle').textContent = ta.hidden ? '+ Add comment' : '- Hide comment';
    },

    submit: function () {
      var date = document.getElementById('ee-date').value;
      var from = document.getElementById('ee-from').value;
      var to = document.getElementById('ee-to').value;
      var taskName = document.getElementById('ee-task').value;
      var taskId = resolveTaskId(taskName, this.taskId);
      if (!taskId) {
        ManualTime.showError('Please pick a task');
        return;
      }
      var btn = document.getElementById('ee-update');
      btn.disabled = true;
      btn.textContent = 'Updating…'; // v3-01841
      var self = this;
      fetch('/api/time/entries/' + encodeURIComponent(this.entryId), {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          task_id: taskId,
          date: date,
          from: from,
          to: to,
          note: document.getElementById('ee-comment').value,
        }),
      })
        .then(function (res) {
          return res.json().then(function (body) { return { ok: res.ok, body: body }; });
        })
        .then(function (result) {
          btn.disabled = false;
          btn.textContent = 'Update';
          if (result.ok) {
            self.close();
            toast('Entry updated.');
            refreshModalTimeLog();
          } else {
            ManualTime.showError(result.body.error || 'Could not update entry.');
          }
        })
        .catch(function () {
          btn.disabled = false;
          btn.textContent = 'Update';
          ManualTime.showError('Could not update entry.');
        });
    },
  };

  function refreshModalTimeLog() {
    var log = document.getElementById('time-entries');
    var taskId = log && log.dataset.taskId;
    if (!taskId) return;
    fetch('/api/tasks/' + encodeURIComponent(taskId) + '/time')
      .then(function (res) { return res.ok ? res.text() : Promise.reject(); })
      .then(function (html) { log.innerHTML = html; })
      .catch(function () {});
  }

  // Per-entry Edit buttons (v3-01841): delegated since the log re-renders.
  document.addEventListener('click', function (e) {
    var btn = e.target.closest('.entry-edit');
    if (!btn) return;
    var entryId = btn.dataset.entryId;
    fetch('/api/time/entries/' + encodeURIComponent(entryId))
      .then(function (res) { return res.ok ? res.json() : Promise.reject(); })
      .then(function (entry) {
        // started_at is RFC3339; the dialog wants "YYYY-MM-DDTHH:MM".
        EditEntry.open(entryId, {
          taskId: entry.task_id,
          taskName: entry.task_name,
          startedAt: (entry.started_at || '').slice(0, 16),
          minutes: entry.minutes,
          note: entry.note,
        });
      })
      .catch(function () { toast('Could not load entry.'); });
  });

  // Wire the dialog controls once the DOM is ready.
  document.addEventListener('DOMContentLoaded', function () {
    var bind = function (id, ev, fn) {
      var el = document.getElementById(id);
      if (el) el.addEventListener(ev, fn);
    };
    bind('mt-from', 'input', function () { ManualTime.updateDuration(); });
    bind('mt-to', 'input', function () { ManualTime.updateDuration(); });
    bind('mt-cal-btn', 'click', function (e) { e.stopPropagation(); ManualTime.openCalendar(); });
    bind('mt-comment-toggle', 'click', function () { ManualTime.toggleComment(); });
    bind('mt-add', 'click', function () { ManualTime.submit(); });
    bind('ee-from', 'input', function () { EditEntry.updateDuration(); });
    bind('ee-to', 'input', function () { EditEntry.updateDuration(); });
    bind('ee-cal-btn', 'click', function (e) { e.stopPropagation(); EditEntry.openCalendar(); });
    bind('ee-comment-toggle', 'click', function () { EditEntry.toggleComment(); });
    bind('ee-update', 'click', function () { EditEntry.submit(); });
    // Clicking a calendar day is handled by the calendar itself; any other
    // click outside the popup closes it.
    document.addEventListener('click', function (e) {
      ['mt-cal-popup', 'ee-cal-popup'].forEach(function (pid) {
        var p = document.getElementById(pid);
        if (p && !p.hidden && !p.contains(e.target)) p.hidden = true;
      });
    });
  });

  function escapeHtml(s) {
    return String(s)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  // ---------- add-task affordance ----------

  function showAddForm(btn) {
    btn.hidden = true;
    var form = btn.closest('.cell').querySelector('.add-task-form');
    form.hidden = false;
    var input = form.querySelector('input[name="name"]');
    if (input) input.focus();
  }

  function hideAddForm(btn) {
    var form = btn.closest('form');
    form.reset();
    form.hidden = true;
    form.closest('.cell').querySelector('.add-task-btn').hidden = false;
  }

  // ---------- column & swimlane management ----------

  function boardId() {
    var main = document.querySelector('main.board');
    return main ? main.dataset.boardId : null;
  }

  function api(path, method, data) {
    return fetch(path, {
      method: method,
      headers: { 'Content-Type': 'application/json' },
      body: data === undefined ? undefined : JSON.stringify(data),
    });
  }

  function alertOnError(res) {
    res.text().then(function (text) {
      window.alert('Request failed (' + res.status + '): ' + text);
    }).catch(function () {
      window.alert('Request failed (' + res.status + ').');
    });
  }

  function colHeaderOf(btn) { return btn.closest('.col-header'); }
  function bandOf(btn) { return btn.closest('.swimlane-band'); }

  function addColumn() {
    var name = window.prompt('New column name:');
    if (!name || !name.trim()) return;
    api('/api/columns', 'POST', { board_id: boardId(), name: name.trim() })
      .then(function (res) { if (res.ok) window.location.reload(); else alertOnError(res); });
  }

  function renameColumn(btn) {
    var h = colHeaderOf(btn);
    var name = window.prompt('Rename column:', h.dataset.columnName);
    if (name === null || !name.trim() || name.trim() === h.dataset.columnName) return;
    api('/api/columns/' + encodeURIComponent(h.dataset.columnId), 'PATCH', { name: name.trim() })
      .then(function (res) { if (res.ok) window.location.reload(); else alertOnError(res); });
  }

  function setWipLimit(btn) {
    var h = colHeaderOf(btn);
    var raw = window.prompt('WIP limit (leave empty to clear):', h.dataset.wipLimit || '');
    if (raw === null) return;
    raw = raw.trim();
    var wip = null;
    if (raw !== '') {
      wip = parseInt(raw, 10);
      if (!wip || wip < 1) { window.alert('Enter a positive number, or leave empty to clear.'); return; }
    }
    api('/api/columns/' + encodeURIComponent(h.dataset.columnId), 'PATCH', { wip_limit: wip })
      .then(function (res) { if (res.ok) window.location.reload(); else alertOnError(res); });
  }

  function moveColumn(btn, dir) {
    var h = colHeaderOf(btn);
    var headers = Array.prototype.slice.call(document.querySelectorAll('.col-header'));
    var index = headers.indexOf(h);
    if (index < 0) return;
    api('/api/columns/' + encodeURIComponent(h.dataset.columnId) + '/move', 'POST', { position: index + dir })
      .then(function (res) { if (res.ok) window.location.reload(); else alertOnError(res); });
  }

  function toggleDoneColumn(btn, isDone) {
    var h = colHeaderOf(btn);
    api('/api/columns/' + encodeURIComponent(h.dataset.columnId), 'PATCH', { is_done: isDone })
      .then(function (res) { if (res.ok) window.location.reload(); else alertOnError(res); });
  }

  function deleteColumn(btn) {
    var h = colHeaderOf(btn);
    var count = parseInt(h.dataset.taskCount, 10) || 0;
    var msg = count > 0
      ? 'Delete column "' + h.dataset.columnName + '"? It still holds ' + count +
        ' task(s) — the server will refuse until they are moved or deleted.'
      : 'Delete column "' + h.dataset.columnName + '"?';
    if (!window.confirm(msg)) return;
    api('/api/columns/' + encodeURIComponent(h.dataset.columnId), 'DELETE')
      .then(function (res) { if (res.ok) window.location.reload(); else alertOnError(res); });
  }

  function addSwimlane() {
    var name = window.prompt('New swimlane name:');
    if (!name || !name.trim()) return;
    api('/api/swimlanes', 'POST', { board_id: boardId(), name: name.trim() })
      .then(function (res) { if (res.ok) window.location.reload(); else alertOnError(res); });
  }

  function renameSwimlane(btn) {
    var band = bandOf(btn);
    var name = window.prompt('Rename swimlane:', band.dataset.swimlaneName);
    if (name === null || !name.trim() || name.trim() === band.dataset.swimlaneName) return;
    api('/api/swimlanes/' + encodeURIComponent(band.dataset.swimlaneId), 'PATCH', { name: name.trim() })
      .then(function (res) { if (res.ok) window.location.reload(); else alertOnError(res); });
  }

  function moveSwimlane(btn, dir) {
    var band = bandOf(btn);
    var bands = Array.prototype.slice.call(document.querySelectorAll('.swimlane-band'));
    var index = bands.indexOf(band);
    if (index < 0) return;
    api('/api/swimlanes/' + encodeURIComponent(band.dataset.swimlaneId) + '/move', 'POST', { position: index + dir })
      .then(function (res) { if (res.ok) window.location.reload(); else alertOnError(res); });
  }

  function deleteSwimlane(btn) {
    var band = bandOf(btn);
    var count = parseInt(band.dataset.taskCount, 10) || 0;
    var msg = count > 0
      ? 'Delete swimlane "' + band.dataset.swimlaneName + '"? It still holds ' + count +
        ' task(s) — the server will refuse until they are moved or deleted.'
      : 'Delete swimlane "' + band.dataset.swimlaneName + '"?';
    if (!window.confirm(msg)) return;
    api('/api/swimlanes/' + encodeURIComponent(band.dataset.swimlaneId), 'DELETE')
      .then(function (res) { if (res.ok) window.location.reload(); else alertOnError(res); });
  }

  // ---------- boot ----------

  // Card clicks open the modal (but not while dragging, and not from
  // interactive elements inside a card).
  document.addEventListener('click', function (e) {
    if (dragging) return;
    if (e.target.closest('button, a, input, select, textarea, form, .modal, .timer-popup, .why-stop-menu')) return;
    var card = e.target.closest('.task-card');
    if (card && card.dataset.taskId) openModal(card.dataset.taskId);
  });

  // Keyboard: Enter on a focused card opens it too.
  document.addEventListener('keydown', function (e) {
    if (e.key !== 'Enter') return;
    var active = document.activeElement;
    if (active && active.classList && active.classList.contains('task-card')) {
      openModal(active.dataset.taskId);
    }
  });

  // Timer menu in the modal rail closes when clicking elsewhere.
  document.addEventListener('click', function (e) {
    var menu = document.getElementById('timer-menu');
    if (menu && !menu.hidden && !e.target.closest('.km-rail-btn-wrap')) menu.hidden = true;
  });

  document.addEventListener('DOMContentLoaded', function () {
    initSortable();
    TimerUI.init();
  });
  if (document.querySelector('.task-list')) initSortable(); // in case DOMContentLoaded already fired
  TimerUI.init();

  // Called from inline onclick handlers in the templates.
  window.showAddForm = showAddForm;
  window.hideAddForm = hideAddForm;
  window.closeModal = closeModal;
  window.saveModalTask = saveModalTask;
  window.deleteModalTask = deleteModalTask;
  window.toggleTimerMenu = toggleTimerMenu;
  window.scrollToTimeLog = scrollToTimeLog;
  window.addColumn = addColumn;
  window.renameColumn = renameColumn;
  window.setWipLimit = setWipLimit;
  window.moveColumn = moveColumn;
  window.toggleDoneColumn = toggleDoneColumn;
  window.deleteColumn = deleteColumn;
  window.addSwimlane = addSwimlane;
  window.renameSwimlane = renameSwimlane;
  window.moveSwimlane = moveSwimlane;
  window.deleteSwimlane = deleteSwimlane;
  window.TimerUI = TimerUI;
})();
