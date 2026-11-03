(function () {
  var pages = {};

  pages['login'] = function () {
    var button = document.getElementById('passkey-sign-in');
    var error = document.getElementById('passkey-error');
    button.addEventListener('click', function () {
      button.disabled = true;
      window.pombPasskey.attempt(error, 'Sign-in failed: ', function () {
        return window.pombPasskey.signIn().then(function (done) { location.assign(done.redirect); });
      }).then(function (signedIn) { button.disabled = signedIn; });
    });
  };

  pages['signup'] = function () {
    var form = document.getElementById('signup-form');
    var error = document.getElementById('passkey-error');
    form.addEventListener('submit', function (e) {
      e.preventDefault();
      var start = { handle: document.getElementById('signup-handle').value.trim() };
      if (form.dataset.invite) start.invite = form.dataset.invite;
      var finish = { name: document.getElementById('signup-passkey-name').value.trim() };
      window.pombPasskey.attempt(error, 'Sign-up failed: ', function () {
        return window.pombPasskey.register('/api/v1/auth/register/options', '/api/v1/auth/register', start, finish)
          .then(function (done) { window.pombPasskey.finish(form, done); });
      });
    });
  };

  pages['setup'] = function () {
    var form = document.getElementById('setup-form');
    var error = document.getElementById('passkey-error');
    form.addEventListener('submit', function (e) {
      e.preventDefault();
      var username = document.getElementById('setup-username');
      var start = username ? { username: username.value.trim() } : {};
      start.code = document.getElementById('setup-code').value.trim();
      var finish = { name: document.getElementById('setup-passkey-name').value.trim() };
      window.pombPasskey.attempt(error, 'Setup failed: ', function () {
        return window.pombPasskey.register('/api/v1/auth/setup/options', '/api/v1/auth/setup', start, finish)
          .then(function (done) { window.pombPasskey.finish(form, done); });
      });
    });
  };

  pages['recover'] = function () {
    var form = document.getElementById('recover-form');
    var error = document.getElementById('passkey-error');
    form.addEventListener('submit', function (e) {
      e.preventDefault();
      var start = {
        handle: document.getElementById('recover-handle').value.trim(),
        recovery_code: document.getElementById('recover-code').value.trim()
      };
      var finish = { name: document.getElementById('recover-passkey-name').value.trim() };
      window.pombPasskey.attempt(error, 'Recovery failed: ', function () {
        return window.pombPasskey.register('/api/v1/auth/recover/options', '/api/v1/auth/recover', start, finish)
          .then(function (done) { window.pombPasskey.finish(form, done); });
      });
    });
  };

  pages['archived-boards'] = function () {
    var err = document.getElementById('archived-error');
    document.querySelectorAll('[data-unarchive]').forEach(function (btn) {
      btn.addEventListener('click', function () {
        var id = btn.getAttribute('data-unarchive');
        btn.disabled = true;
        fetch('/api/boards/' + encodeURIComponent(id) + '/config', {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          credentials: 'same-origin',
          body: JSON.stringify({ archived: false })
        })
          .then(function (r) {
            if (!r.ok) return r.text().then(function (t) { throw new Error(t || ('HTTP ' + r.status)); });
            var row = btn.closest('.archived-board-row');
            if (row) row.remove();
            if (!document.querySelector('.archived-board-row')) location.reload();
          })
          .catch(function (ex) {
            btn.disabled = false;
            err.textContent = 'Could not unarchive board: ' + ex.message;
            err.hidden = false;
          });
      });
    });
  };

  pages['admin'] = function () {
    var panes = document.querySelectorAll('.settings-pane');
    var navItems = document.querySelectorAll('.bshell-nav-item');
    function showPane() {
      var id = location.hash.slice(1);
      var known = false;
      panes.forEach(function (p) { if (p.id === id) known = true; });
      if (!known) id = 'accounts';
      panes.forEach(function (p) { p.hidden = p.id !== id; });
      navItems.forEach(function (a) { a.classList.toggle('bshell-nav-active', a.dataset.pane === id); });
    }
    window.addEventListener('hashchange', showPane);
    showPane();
    function jsonRequest(method, url, body) {
      return window.pombPasskey.fetch(url, {
        method: method,
        headers: { 'Content-Type': 'application/json' },
        body: body === undefined ? undefined : JSON.stringify(body)
      }).then(function (r) {
        if (!r.ok) return r.text().then(function (t) { throw new Error(t || ('HTTP ' + r.status)); });
        return r.json();
      });
    }
    function failed(element, prefix) {
      return function (ex) { element.textContent = prefix + ex.message; element.hidden = false; };
    }
    function reloadAt(pane) { location.hash = pane; location.reload(); }
    function confirmThen(title, message, label, action) {
      document.getElementById('confirm-title').textContent = title;
      document.getElementById('confirm-message').textContent = message;
      var okBtn = document.getElementById('confirm-ok');
      var fresh = okBtn.cloneNode(true);
      fresh.textContent = label;
      okBtn.parentNode.replaceChild(fresh, okBtn);
      fresh.addEventListener('click', function () {
        document.getElementById('confirm-dialog').hidden = true;
        action();
      });
      document.getElementById('confirm-dialog').hidden = false;
      fresh.focus();
    }
    var accountError = document.getElementById('account-error');
    function accountRequest(method, url) {
      accountError.hidden = true;
      jsonRequest(method, url)
        .then(function () { reloadAt('accounts'); })
        .catch(failed(accountError, 'Could not change the account: '));
    }
    document.querySelectorAll('.account-action').forEach(function (btn) {
      btn.addEventListener('click', function () {
        var url = '/api/v1/admin/accounts/' + encodeURIComponent(btn.dataset.id) + '/' + btn.dataset.action;
        if (btn.dataset.action !== 'suspend') {
          accountRequest('POST', url);
          return;
        }
        confirmThen('Suspend account',
          'Suspend "' + btn.dataset.handle + '"? It is signed out, its tokens stop working until you unsuspend it, and its boards stay.',
          'Suspend', function () { accountRequest('POST', url); });
      });
    });
    document.querySelectorAll('.account-delete').forEach(function (btn) {
      btn.addEventListener('click', function () {
        confirmThen('Delete account',
          'Delete "' + btn.dataset.handle + '" and everything it owns? This cannot be undone.',
          'Delete', function () {
            accountRequest('DELETE', '/api/v1/admin/accounts/' + encodeURIComponent(btn.dataset.id));
          });
      });
    });
    var registrationForm = document.getElementById('registration-form');
    var registrationError = document.getElementById('registration-error');
    var limitsForm = document.getElementById('limits-form');
    var limitsError = document.getElementById('limits-error');
    var pruningForm = document.getElementById('pruning-form');
    var pruningError = document.getElementById('pruning-error');
    function limitValue(name) {
      var raw = limitsForm.elements[name].value.trim();
      return raw === '' ? null : parseInt(raw, 10);
    }
    var stored = JSON.parse(limitsForm.dataset.settings);
    function limitsSection() {
      return {
        quotas: {
          boards_per_account: limitValue('boards_per_account'),
          tasks_per_board: limitValue('tasks_per_board'),
          comments_per_task: limitValue('comments_per_task'),
          attachments_per_task: limitValue('attachments_per_task'),
          subtasks_per_task: limitValue('subtasks_per_task'),
          templates_per_account: limitValue('templates_per_account'),
          stored_mib_per_account: limitValue('stored_mib_per_account'),
          stored_mib_per_board: limitValue('stored_mib_per_board'),
          columns_per_board: limitValue('columns_per_board'),
          swimlanes_per_board: limitValue('swimlanes_per_board'),
          fields_per_board: limitValue('fields_per_board'),
          time_entries_per_account: limitValue('time_entries_per_account')
        },
        limits: {
          token_burst: limitValue('token_burst'),
          token_per_second: limitValue('token_per_second'),
          account_burst: limitValue('account_burst'),
          account_per_second: limitValue('account_per_second'),
          ip_per_minute: limitValue('ip_per_minute')
        },
        disk: {
          max_database_mib: limitValue('max_database_mib'),
          min_free_mib: limitValue('min_free_mib'),
          max_attachment_mib: limitValue('max_attachment_mib')
        }
      };
    }
    function saveSettings(section, pane, errorElement, prefix) {
      errorElement.hidden = true;
      jsonRequest('PUT', '/api/v1/admin/settings', Object.assign({}, stored, section))
        .then(function () { reloadAt(pane); })
        .catch(failed(errorElement, prefix));
    }
    registrationForm.addEventListener('submit', function (e) {
      e.preventDefault();
      saveSettings({
        registration: registrationForm.querySelector('input[name="registration"]:checked').value,
        pending_cap: parseInt(document.getElementById('pending-cap').value, 10)
      }, 'registration', registrationError, 'Could not save registration: ');
    });
    var tipsForm = document.getElementById('tips-form');
    var tipsError = document.getElementById('tips-error');
    tipsForm.addEventListener('submit', function (e) {
      e.preventDefault();
      saveSettings({
        disable_tips: tipsForm.elements.disable_tips.checked
      }, 'tips', tipsError, 'Could not save the tips setting: ');
    });
    limitsForm.addEventListener('submit', function (e) {
      e.preventDefault();
      saveSettings(limitsSection(), 'limits', limitsError, 'Could not save limits: ');
    });
    pruningForm.addEventListener('submit', function (e) {
      e.preventDefault();
      saveSettings({
        prune: {
          enabled: pruningForm.elements.prune_enabled.checked,
          inactive_days: parseInt(pruningForm.elements.inactive_days.value, 10),
          warning_days: parseInt(pruningForm.elements.warning_days.value, 10)
        }
      }, 'pruning', pruningError, 'Could not save the unused-board settings: ');
    });
    var inviteForm = document.getElementById('invite-create-form');
    var inviteError = document.getElementById('invite-error');
    inviteForm.addEventListener('submit', function (e) {
      e.preventDefault();
      inviteError.hidden = true;
      jsonRequest('POST', '/api/v1/admin/invites', {
        expires_in_days: parseInt(document.getElementById('invite-expiry').value, 10)
      }).then(function (data) {
        document.getElementById('invite-created-value').textContent = data.url;
        document.getElementById('invite-created').hidden = false;
      }).catch(failed(inviteError, 'Could not create an invite: '));
    });
    document.querySelectorAll('.invite-revoke').forEach(function (btn) {
      btn.addEventListener('click', function () {
        confirmThen('Revoke invite', 'Revoke this invite link? It will no longer create an account.',
          'Revoke', function () {
            inviteError.hidden = true;
            jsonRequest('DELETE', '/api/v1/admin/invites/' + encodeURIComponent(btn.dataset.id))
              .then(function () { reloadAt('invites'); })
              .catch(failed(inviteError, 'Could not revoke the invite: '));
          });
      });
    });
  };

  pages['device'] = function () {
    var form = document.getElementById('device-approve');
    if (!form) return;
    var error = document.getElementById('device-error');
    form.addEventListener('submit', function (e) {
      e.preventDefault();
      window.pombPasskey.attempt(error, 'Could not approve: ', function () {
        return window.pombPasskey.fetch('/device/approve', {
          method: 'POST',
          body: new URLSearchParams(new FormData(form))
        }).then(function (r) {
          return r.text().then(function (text) {
            var page = new DOMParser().parseFromString(text, 'text/html').querySelector('main');
            if (!page) throw new Error(text || ('HTTP ' + r.status));
            document.querySelector('main').replaceWith(page);
          });
        });
      }, false);
    });
  };

  pages['new-board'] = function () {
    'use strict';
    var CREATE_FAILED = 'Could not create the board.';
    function showError(msg) {
      var el = document.getElementById('nb-error');
      el.textContent = msg;
      el.hidden = false;
    }
    var copyRadio = document.getElementById('nb-copy-radio');
    var copyFields = document.getElementById('nb-copy-fields');
    var copyBoard = document.getElementById('nb-copy-board');
    var copyName = document.getElementById('nb-copy-name');
    var copyTasks = document.getElementById('nb-copy-tasks');
    var copyClear = document.getElementById('nb-copy-clear');
    function copyNameFor() {
      var option = copyBoard.options[copyBoard.selectedIndex];
      return option ? option.textContent + ' copy' : '';
    }
    if (copyRadio) {
      var radios = document.querySelectorAll('input[name="template_id"]');
      Array.prototype.forEach.call(radios, function (radio) {
        radio.addEventListener('change', function () {
          copyFields.hidden = !copyRadio.checked;
          if (copyRadio.checked && !copyName.value) copyName.value = copyNameFor();
        });
      });
      copyBoard.addEventListener('change', function () { copyName.value = copyNameFor(); });
      copyTasks.addEventListener('change', function () {
        copyClear.disabled = !copyTasks.checked;
        if (!copyTasks.checked) copyClear.checked = false;
      });
    }
    var selected = document.getElementById('nb-selected');
    function showSelected() {
      var picked = document.querySelector('input[name="template_id"]:checked');
      var title = picked && picked.closest('.nb-card').querySelector('strong');
      selected.textContent = title ? 'Selected layout: ' + title.textContent : '';
    }
    Array.prototype.forEach.call(document.querySelectorAll('input[name="template_id"]'), function (radio) {
      radio.addEventListener('change', showSelected);
    });
    showSelected();
    var chips = document.querySelectorAll('[data-nb-chip]');
    Array.prototype.forEach.call(chips, function (chip) {
      chip.addEventListener('click', function () {
        var wanted = chip.getAttribute('data-nb-chip');
        Array.prototype.forEach.call(chips, function (other) {
          other.classList.toggle('nb-chip-on', other === chip);
        });
        Array.prototype.forEach.call(document.querySelectorAll('[data-nb-panel]'), function (panel) {
          panel.hidden = wanted !== '' && panel.getAttribute('data-nb-panel') !== wanted;
        });
      });
    });
    document.getElementById('nb-create').addEventListener('click', function () {
      var copying = !!copyRadio && copyRadio.checked;
      var name = (copying ? copyName : document.getElementById('nb-name')).value.trim();
      if (!name) { showError(copying ? 'Name of copy is required.' : 'Board name is required.'); return; }

      // Count characters as the server does, not UTF-16 units.
      if (Array.from(name).length > 100) { showError('Board name must be 1-100 characters.'); return; }
      var picked = document.querySelector('input[name="template_id"]:checked');
      var templateId = picked && picked.value ? picked.value : null;
      var payload = { name: name, template_id: templateId };
      if (copying) {
        payload = {
          name: name,
          copy_board_id: copyBoard.value,
          copy_tasks: copyTasks.checked,
          clear_members: copyClear.checked,
        };
      }
      fetch('/api/boards', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
        body: JSON.stringify(payload),
      }).then(function (res) {
        if (res.ok) return res.json();
        return res.text().then(function (text) {
          var message = '';
          try { message = JSON.parse(text).message || ''; } catch (ignored) { message = ''; }
          showError(message || CREATE_FAILED);
          return null;
        });
      }).then(function (data) {
        if (data && data.id) window.location.href = '/b/' + encodeURIComponent(data.id);
      }).catch(function () { showError(CREATE_FAILED); });
    });
    document.getElementById('nb-name').addEventListener('keydown', function (e) {
      if (e.key === 'Enter') document.getElementById('nb-create').click();
    });

    var importButton = document.getElementById('nb-import');
    var importError = document.getElementById('nb-import-error');
    function failImport(msg) {
      importError.textContent = msg;
      importError.hidden = false;
      importButton.disabled = false;
    }

    function refusalText(text, status) {
      var message = text;
      try { message = JSON.parse(text).message || text; } catch (ignored) { message = text; }
      return message || ('HTTP ' + status);
    }
    importButton.addEventListener('click', function () {
      var file = document.getElementById('nb-import-file').files[0];
      importError.hidden = true;
      if (!file) { failImport('Choose a board archive first.'); return; }
      importButton.disabled = true;
      fetch('/api/boards/import', {
        method: 'POST',
        headers: { 'Content-Type': 'application/gzip' },
        body: file,
      }).then(function (res) {
        if (res.ok) return res.json();
        return res.text().then(function (text) { throw new Error(refusalText(text, res.status)); });
      }).then(function (data) {
        if (data && data.id) window.location.href = '/b/' + encodeURIComponent(data.id);
      }).catch(function (ex) { failImport('Could not import the board: ' + ex.message); });
    });
  };

  pages['new-board-templates'] = function () {
    'use strict';
    var pendingTemplateId = null;

    // This page keeps its own copy of the error helper.
    function showError(msg) {
      var el = document.getElementById('nb-error');
      el.textContent = msg;
      el.hidden = false;
    }
    function closeTplDialog() {
      document.getElementById('tpl-confirm-dialog').hidden = true;
      pendingTemplateId = null;
    }
    document.querySelectorAll('[data-close-tpl-dialog]').forEach(function (btn) {
      btn.addEventListener('click', closeTplDialog);
    });
    document.getElementById('tpl-confirm-dialog').addEventListener('click', function (e) {
      if (e.target === this) closeTplDialog();
    });
    document.getElementById('tpl-confirm-ok').addEventListener('click', function () {
      var id = pendingTemplateId;
      closeTplDialog();
      if (!id) return;
      fetch('/api/templates/' + encodeURIComponent(id), { method: 'DELETE' })
        .then(function (res) {
          if (res.ok) window.location.reload();
          else showError('Could not delete the template.');
        })
        .catch(function () { showError('Could not delete the template.'); });
    });

    // Wire the delete buttons to open the dialog instead of window.confirm().
    document.querySelectorAll('[data-delete-template]').forEach(function (btn) {
      btn.addEventListener('click', function (e) {
        e.preventDefault();
        e.stopImmediatePropagation();
        pendingTemplateId = btn.getAttribute('data-delete-template');
        document.getElementById('tpl-confirm-dialog').hidden = false;
        document.getElementById('tpl-confirm-ok').focus();
      }, true);
    });
  };

  var TokenForm = {
    boardsAll: 'all',
    boardsSome: 'some',
    groups: function (catalog) {
      var order = [];
      var byName = {};
      catalog.forEach(function (item) {
        if (!byName[item.group]) {
          byName[item.group] = { name: item.group, items: [] };
          order.push(byName[item.group]);
        }
        byName[item.group].items.push(item);
      });
      return order;
    },
    presets: function (catalog) {
      var held = {};
      catalog.forEach(function (item) {
        item.presets.forEach(function (name) {
          (held[name] = held[name] || []).push(item.name);
        });
      });
      return Object.keys(held)
        .map(function (name) { return { name: name, permissions: held[name] }; })
        .sort(function (a, b) { return a.permissions.length - b.permissions.length; });
    },
    body: function (catalog, form) {
      return {
        name: form.name,
        permissions: catalog
          .map(function (item) { return item.name; })
          .filter(function (name) { return form.ticked.indexOf(name) >= 0; }),
        boards: form.boards === null ? TokenForm.boardsAll : form.boards,
        expires_in_days: form.expiryDays
      };
    }
  };

  pages['settings'] = function () {
    var panes = document.querySelectorAll('.settings-pane');
    var navItems = document.querySelectorAll('.bshell-nav-item');
    function showPane() {
      var id = location.hash.slice(1);
      var known = false;
      panes.forEach(function (p) { if (p.id === id) known = true; });
      if (!known) id = 'profile';
      panes.forEach(function (p) { p.hidden = p.id !== id; });
      navItems.forEach(function (a) { a.classList.toggle('bshell-nav-active', a.dataset.pane === id); });
    }
    window.addEventListener('hashchange', showPane);
    showPane();
    var profileForm = document.getElementById('profile-form');
    var profileError = document.getElementById('profile-error');
    var profileSaved = document.getElementById('profile-saved');
    profileForm.addEventListener('submit', function (e) {
      e.preventDefault();
      profileError.hidden = true;
      profileSaved.hidden = true;
      fetch('/api/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ full_name: document.getElementById('profile-full-name').value })
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        profileSaved.hidden = false;
      }).catch(function (ex) {
        profileError.textContent = 'Could not save the full name: ' + ex.message;
        profileError.hidden = false;
      });
    });
    document.getElementById('time-zone-value').textContent =
      Intl.DateTimeFormat().resolvedOptions().timeZone;
    var dateTimeError = document.getElementById('date-time-error');
    document.querySelectorAll('[data-open-dialog]').forEach(function (button) {
      button.addEventListener('click', function () {
        dateTimeError.hidden = true;
        document.getElementById(button.dataset.openDialog).hidden = false;
      });
    });
    var showTips = document.getElementById('show-tips');
    showTips.addEventListener('change', function () {
      fetch('/api/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ show_tips: showTips.checked })
      }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
      }).catch(function (ex) {
        showTips.checked = !showTips.checked;
        profileError.textContent = 'Could not save the tips setting: ' + ex.message;
        profileError.hidden = false;
      });
    });
    document.querySelectorAll('.date-time-form').forEach(function (form) {
      form.addEventListener('submit', function (e) {
        e.preventDefault();
        var patch = {};
        patch[form.dataset.setting] = form.elements[form.dataset.setting].value;
        fetch('/api/settings', {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          credentials: 'same-origin',
          body: JSON.stringify(patch)
        }).then(function (r) {
          if (!r.ok) throw new Error('HTTP ' + r.status);
          location.reload();
        }).catch(function (ex) {
          form.closest('.dlg-overlay').hidden = true;
          dateTimeError.textContent = 'Could not save the setting: ' + ex.message;
          dateTimeError.hidden = false;
        });
      });
    });
    var avatarDialog = document.getElementById('avatar-dialog');
    var avatarForm = document.getElementById('avatar-form');
    var avatarError = document.getElementById('avatar-error');
    function avatarKind() {
      return avatarForm.elements['avatar-kind'].value;
    }
    function showAvatarFields() {
      document.getElementById('avatar-text-fields').hidden = avatarKind() !== 'text';
      document.getElementById('avatar-image-fields').hidden = avatarKind() !== 'image';
    }
    avatarForm.querySelectorAll('input[name="avatar-kind"]').forEach(function (radio) {
      radio.addEventListener('change', showAvatarFields);
    });
    showAvatarFields();
    document.getElementById('avatar-change').addEventListener('click', function () {
      avatarError.hidden = true;
      avatarDialog.hidden = false;
    });
    function avatarSaveFailed(ex) {
      avatarError.textContent = 'Could not save the avatar: ' + ex.message;
      avatarError.hidden = false;
    }
    function avatarSaved(r) {
      if (r.ok) return location.reload();
      return r.json().catch(function () { return {}; }).then(function (body) {
        throw new Error(body.message || ('HTTP ' + r.status));
      });
    }
    avatarForm.addEventListener('submit', function (e) {
      e.preventDefault();
      avatarError.hidden = true;
      if (avatarKind() === 'text') {
        fetch('/api/settings', {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          credentials: 'same-origin',
          body: JSON.stringify({
            avatar: {
              text: document.getElementById('avatar-text').value,
              text_color: document.getElementById('avatar-text-color').value,
              background: document.getElementById('avatar-background').value
            }
          })
        }).then(avatarSaved).catch(avatarSaveFailed);
        return;
      }
      var file = document.getElementById('avatar-file').files[0];
      if (!file) {
        avatarSaveFailed(new Error('select an image file'));
        return;
      }
      fetch('/api/avatar', {
        method: 'PUT',
        headers: { 'Content-Type': file.type },
        credentials: 'same-origin',
        body: file
      }).then(avatarSaved).catch(avatarSaveFailed);
    });
    var toggles = document.querySelectorAll('.notification-toggle');
    var notificationError = document.getElementById('notification-error');
    toggles.forEach(function (toggle) {
      toggle.addEventListener('change', function () {
        var muted = [];
        toggles.forEach(function (other) { if (!other.checked) muted.push(other.dataset.kind); });
        notificationError.hidden = true;
        fetch('/api/settings', {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          credentials: 'same-origin',
          body: JSON.stringify({ muted_notifications: muted })
        }).then(function (r) {
          if (!r.ok) throw new Error('HTTP ' + r.status);
        }).catch(function (ex) {
          toggle.checked = !toggle.checked;
          notificationError.textContent = 'Could not save the notification setting: ' + ex.message;
          notificationError.hidden = false;
        });
      });
    });
    var form = document.getElementById('token-create-form');
    var err = document.getElementById('token-error');
    function showError(msg) { err.textContent = msg; err.hidden = false; }
    var catalog = [];
    var presetsBox = document.getElementById('token-presets');
    var permsBox = document.getElementById('token-permissions');
    var boardsBox = document.getElementById('token-board-list');
    var boardMode = document.querySelectorAll('input[name="token-boards"]');
    function checkedValues(box) {
      return Array.prototype.map.call(box.querySelectorAll('input:checked'), function (input) {
        return input.value;
      });
    }
    function applyPreset(names) {
      permsBox.querySelectorAll('input').forEach(function (input) {
        input.checked = names.indexOf(input.value) >= 0;
      });
    }
    function checkbox(value, text, title) {
      var label = document.createElement('label');
      label.className = 'check-label';
      var input = document.createElement('input');
      input.type = 'checkbox';
      input.value = value;
      label.appendChild(input);
      label.appendChild(document.createTextNode(' ' + text));
      if (title) label.title = title;
      return label;
    }
    function renderCatalog(items) {
      catalog = items;
      presetsBox.textContent = '';
      var presets = TokenForm.presets(items);
      presets.forEach(function (preset, at) {
        var label = document.createElement('label');
        label.className = 'check-label';
        var radio = document.createElement('input');
        radio.type = 'radio';
        radio.name = 'token-preset';
        radio.value = preset.name;
        radio.checked = at === 0;
        radio.addEventListener('change', function () { applyPreset(preset.permissions); });
        label.appendChild(radio);
        label.appendChild(document.createTextNode(' ' + preset.name + ' (' + preset.permissions.length + ' permissions)'));
        presetsBox.appendChild(label);
      });
      permsBox.textContent = '';
      TokenForm.groups(items).forEach(function (group) {
        var set = document.createElement('fieldset');
        set.className = 'token-group';
        var legend = document.createElement('legend');
        legend.textContent = group.name;
        set.appendChild(legend);
        group.items.forEach(function (item) {
          var box = checkbox(item.name, item.name, item.description);
          box.querySelector('input').addEventListener('change', function () {
            presetsBox.querySelectorAll('input').forEach(function (radio) { radio.checked = false; });
          });
          set.appendChild(box);
        });
        permsBox.appendChild(set);
      });
      if (presets.length) applyPreset(presets[0].permissions);
    }
    fetch('/api/v1/auth/permissions', { headers: { 'Accept': 'application/json' } })
      .then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      })
      .then(renderCatalog)
      .catch(function (ex) { presetsBox.textContent = 'Could not load permissions: ' + ex.message; });
    fetch('/api/boards', { headers: { 'Accept': 'application/json' } })
      .then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      })
      .then(function (boards) {
        boards.forEach(function (board) { boardsBox.appendChild(checkbox(board.id, board.name)); });
      })
      .catch(function (ex) { boardsBox.textContent = 'Could not load boards: ' + ex.message; });
    boardMode.forEach(function (radio) {
      radio.addEventListener('change', function () {
        boardsBox.hidden = document.querySelector('input[name="token-boards"]:checked').value !== TokenForm.boardsSome;
      });
    });
    form.addEventListener('submit', function (e) {
      e.preventDefault();
      err.hidden = true;
      var only = document.querySelector('input[name="token-boards"]:checked').value === TokenForm.boardsSome;
      var request = TokenForm.body(catalog, {
        name: document.getElementById('token-name').value.trim(),
        ticked: checkedValues(permsBox),
        boards: only ? checkedValues(boardsBox) : null,
        expiryDays: parseInt(document.getElementById('token-expiry').value, 10)
      });
      if (!request.permissions.length) { showError('Tick at least one permission.'); return; }
      if (only && !request.boards.length) { showError('Pick at least one board.'); return; }
      window.pombPasskey.fetch('/api/v1/auth/tokens', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request)
      }).then(function (r) {
        if (!r.ok) return r.text().then(function (t) { throw new Error(t || ('HTTP ' + r.status)); });
        return r.json();
      }).then(function (data) {
        document.getElementById('token-created-value').textContent = data.raw_token;
        document.getElementById('token-created').hidden = false;
        form.reset();
        boardsBox.hidden = true;
        boardsBox.querySelectorAll('input').forEach(function (input) { input.checked = false; });
        renderCatalog(catalog);
        document.getElementById('token-expiry').value = '365';

        // Refresh the Active tokens list in place instead of a delayed full-page reload,
        // so the one-time secret above stays visible.
        refreshTokenList();
      }).catch(function (ex) { showError('Could not create token: ' + ex.message); });
    });
    var inspectForm = document.getElementById('token-inspect-form');
    var inspectError = document.getElementById('token-inspect-error');
    var inspectResult = document.getElementById('token-inspect-result');
    inspectForm.addEventListener('submit', function (e) {
      e.preventDefault();
      inspectError.hidden = true;
      inspectResult.hidden = true;
      var input = document.getElementById('token-inspect-input');
      fetch('/api/v1/auth/tokens/inspect', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
        body: JSON.stringify({ token: input.value.trim() })
      }).then(function (r) {
        if (r.status === 404) throw new Error('no such token on this account');
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      }).then(function (data) {
        document.getElementById('token-inspect-name').textContent = data.name;
        document.getElementById('token-inspect-expiry').textContent = data.expires_at
          ? 'expires ' + new Date(data.expires_at * 1000).toISOString().slice(0, 10)
          : 'never expires';
        document.getElementById('token-inspect-boards').textContent = data.boards === TokenForm.boardsAll
          ? 'Boards: all'
          : 'Boards: ' + data.boards.map(function (board) { return board.name; }).join(', ');
        var list = document.getElementById('token-inspect-permissions');
        list.textContent = '';
        data.permissions.forEach(function (name) {
          var item = document.createElement('li');
          item.textContent = name;
          list.appendChild(item);
        });
        inspectResult.hidden = false;
        input.value = '';
      }).catch(function (ex) {
        inspectError.textContent = 'Could not inspect the token: ' + ex.message;
        inspectError.hidden = false;
      });
    });
    document.getElementById('token-copy').addEventListener('click', function () {
      var v = document.getElementById('token-created-value').textContent;
      if (navigator.clipboard) navigator.clipboard.writeText(v);
    });

    // Re-fetch the server-rendered token list and swap it in, so the
    // new token appears immediately without losing the one-time secret.
    function refreshTokenList() {
      fetch('/settings', { headers: { 'Accept': 'text/html' } })
        .then(function (r) {
          if (!r.ok) throw new Error('HTTP ' + r.status);
          return r.text();
        })
        .then(function (html) {
          var doc = new DOMParser().parseFromString(html, 'text/html');
          var fresh = doc.getElementById('token-list');
          var current = document.getElementById('token-list');
          if (fresh && current) {
            current.innerHTML = fresh.innerHTML;
            bindTokenRevokeButtons(current);
          }
        })
        .catch(function (ex) { showError('Could not refresh token list: ' + ex.message); });
    }
    const confirmTokenRevoke = (title, message, url, failure) => {
      document.getElementById('confirm-title').textContent = title;
      document.getElementById('confirm-message').textContent = message;
      const okBtn = document.getElementById('confirm-ok');
      okBtn.textContent = 'Revoke';

      // Replace the button to drop any previously attached listeners.
      const fresh = okBtn.cloneNode(true);
      okBtn.parentNode.replaceChild(fresh, okBtn);
      fresh.addEventListener('click', () => {
        document.getElementById('confirm-dialog').hidden = true;
        fetch(url, { method: 'DELETE' })
          .then((r) => {
            if (!r.ok) throw new Error(`HTTP ${r.status}`);
            location.reload();
          })
          .catch((ex) => showError(failure + ex.message));
      });
      document.getElementById('confirm-dialog').hidden = false;
      fresh.focus();
    };
    function bindTokenRevokeButtons(root) {
      root.querySelectorAll('.token-revoke').forEach((btn) => {
        btn.addEventListener('click', () => {
          confirmTokenRevoke(
            'Revoke token',
            `Revoke token "${btn.dataset.name}"? Anything using it will stop working.`,
            `/api/v1/auth/tokens/${encodeURIComponent(btn.dataset.id)}`,
            'Could not revoke token: ');
        });
      });
      root.querySelectorAll('.token-revoke-children').forEach((btn) => {
        btn.addEventListener('click', () => {
          confirmTokenRevoke(
            'Revoke all children',
            `Revoke every child token of "${btn.dataset.name}"? Anything using them will stop working.`,
            `/api/v1/auth/tokens/${encodeURIComponent(btn.dataset.id)}/children`,
            'Could not revoke the children: ');
        });
      });
    }
    bindTokenRevokeButtons(document);
    var passkeyError = document.getElementById('passkey-error');
    function reloadPasskeys() { location.hash = 'passkeys'; location.reload(); }
    var passkeyForm = document.getElementById('passkey-add-form');
    if (passkeyForm) {
      passkeyForm.addEventListener('submit', function (e) {
        e.preventDefault();
        var finish = { name: document.getElementById('passkey-name').value.trim() };
        window.pombPasskey.attempt(passkeyError, 'Could not add passkey: ', function () {
          return window.pombPasskey.register('/api/v1/auth/passkeys/options', '/api/v1/auth/passkeys', {}, finish)
            .then(reloadPasskeys);
        });
      });
    }
    document.querySelectorAll('.passkey-remove').forEach(function (btn) {
      btn.addEventListener('click', function () {
        document.getElementById('confirm-title').textContent = 'Remove passkey';
        document.getElementById('confirm-message').textContent =
          'Remove passkey "' + btn.dataset.name + '"? It will no longer sign you in.';
        var okBtn = document.getElementById('confirm-ok');
        var fresh = okBtn.cloneNode(true);
        fresh.textContent = 'Remove';
        okBtn.parentNode.replaceChild(fresh, okBtn);
        fresh.addEventListener('click', function () {
          document.getElementById('confirm-dialog').hidden = true;
          window.pombPasskey.attempt(passkeyError, 'Could not remove passkey: ', function () {
            return window.pombPasskey.remove(btn.dataset.id).then(reloadPasskeys);
          }, false);
        });
        document.getElementById('confirm-dialog').hidden = false;
        fresh.focus();
      });
    });
    document.querySelectorAll('.session-revoke').forEach(function (btn) {
      btn.addEventListener('click', function () {
        var result = document.getElementById('session-revoke-result');
        jsonRequest('DELETE', '/api/v1/auth/sessions/' + encodeURIComponent(btn.dataset.id))
          .then(function () { btn.closest('tr').remove(); })
          .catch(failed(result, 'Could not revoke the session: '));
      });
    });
    var signOutOthers = document.getElementById('sign-out-others');
    if (signOutOthers) {
      signOutOthers.addEventListener('click', function () {
        var result = document.getElementById('sign-out-others-result');
        jsonRequest('POST', '/api/v1/auth/sessions/sign-out-others')
          .then(function (data) {
            result.textContent = data.ended === 1
              ? 'Signed out 1 other session.'
              : 'Signed out ' + data.ended + ' other sessions.';
            result.hidden = false;
          })
          .catch(failed(result, 'Could not sign out other sessions: '));
      });
    }
    function jsonRequest(method, url, body) {
      return window.pombPasskey.fetch(url, {
        method: method,
        headers: { 'Content-Type': 'application/json' },
        body: body === undefined ? undefined : JSON.stringify(body)
      }).then(function (r) {
        if (!r.ok) return r.text().then(function (t) { throw new Error(t || ('HTTP ' + r.status)); });
        return r.json();
      });
    }
    function failed(element, prefix) {
      return function (ex) { element.textContent = prefix + ex.message; element.hidden = false; };
    }
    function reloadBackups() { location.hash = 'backups'; location.reload(); }
    var backupForm = document.getElementById('backup-settings-form');
    if (backupForm) {
      var backupError = document.getElementById('backup-error');
      backupForm.addEventListener('submit', function (e) {
        e.preventDefault();
        backupError.hidden = true;
        jsonRequest('PUT', '/api/v1/backups/settings', {
          enabled: document.getElementById('backup-enabled').checked,
          interval_hours: parseInt(document.getElementById('backup-interval').value, 10),
          keep: parseInt(document.getElementById('backup-keep').value, 10)
        }).then(reloadBackups).catch(failed(backupError, 'Could not save backup settings: '));
      });
      document.getElementById('backup-now').addEventListener('click', function () {
        var button = this;
        button.disabled = true;
        backupError.hidden = true;
        jsonRequest('POST', '/api/v1/backups')
          .then(reloadBackups)
          .catch(function (ex) { button.disabled = false; failed(backupError, 'Could not back up: ')(ex); });
      });
    }
    var unnamedRemove = document.getElementById('unnamed-remove');
    if (unnamedRemove) {
      var unnamedBox = document.getElementById('unnamed-files');
      unnamedRemove.addEventListener('click', function () {
        var button = this;
        document.getElementById('confirm-title').textContent = 'Remove unnamed files';
        document.getElementById('confirm-message').textContent =
          'Delete ' + unnamedBox.dataset.count + ' attachment files (' + unnamedBox.dataset.size +
          ') that no task names? This cannot be undone.';
        var okBtn = document.getElementById('confirm-ok');
        okBtn.textContent = 'Remove';
        var fresh = okBtn.cloneNode(true);
        okBtn.parentNode.replaceChild(fresh, okBtn);
        fresh.addEventListener('click', function () {
          document.getElementById('confirm-dialog').hidden = true;
          button.disabled = true;
          backupError.hidden = true;
          jsonRequest('DELETE', '/api/v1/backups/unnamed-files')
            .then(reloadBackups)
            .catch(function (ex) { button.disabled = false; failed(backupError, 'Could not remove the files: ')(ex); });
        });
        document.getElementById('confirm-dialog').hidden = false;
        fresh.focus();
      });
    }
    var storageForm = document.getElementById('storage-form');
    if (storageForm) {
      var storageError = document.getElementById('storage-error');
      storageForm.addEventListener('submit', function (e) {
        e.preventDefault();
        storageError.hidden = true;
        var max = storageForm.dataset.maxDatabaseMib;
        jsonRequest('PUT', '/api/v1/admin/settings', {
          disk: {
            max_database_mib: max === '' ? null : parseInt(max, 10),
            min_free_mib: parseInt(document.getElementById('storage-min-free').value, 10),
            max_attachment_mib: parseInt(document.getElementById('storage-max-attachment').value, 10)
          }
        }).then(function () { location.hash = 'storage'; location.reload(); })
          .catch(failed(storageError, 'Could not save storage settings: '));
      });
    }
    var recoveryButton = document.getElementById('recovery-regenerate');
    if (recoveryButton) {
      var recoveryError = document.getElementById('recovery-error');
      recoveryButton.addEventListener('click', function () {
        recoveryError.hidden = true;
        jsonRequest('POST', '/api/v1/account/recovery-code').then(function (data) {
          document.getElementById('recovery-created-value').textContent = data.recovery_code;
          document.getElementById('recovery-created').hidden = false;
          recoveryButton.textContent = 'Regenerate recovery code';
        }).catch(failed(recoveryError, 'Could not create a recovery code: '));
      });
    }
    var deleteForm = document.getElementById('account-delete-form');
    if (deleteForm) {
      var deleteError = document.getElementById('account-delete-error');
      deleteForm.addEventListener('submit', function (e) {
        e.preventDefault();
        deleteError.hidden = true;
        jsonRequest('POST', '/api/v1/account/delete', {
          handle: document.getElementById('account-delete-handle').value.trim()
        }).then(function (data) { location.assign(data.redirect); })
          .catch(failed(deleteError, 'Could not delete the account: '));
      });
    }
  };

  pages['board-settings-estimate-unit'] = function () {
    'use strict';
    var button = document.getElementById('estimate-unit-switch');
    var error = document.getElementById('estimate-unit-error');
    var boardId = document.querySelector('.board-settings-page').dataset.boardId;
    button.addEventListener('click', function () {
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/config', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ estimate_in_points: button.dataset.points === '1' }),
      }).then(function (res) {
        if (!res.ok) throw new Error('save failed');
        window.location.reload();
      }).catch(function () {
        error.textContent = 'Could not switch the estimation unit.';
        error.hidden = false;
      });
    });
  };

  pages['board-settings-numbering'] = function () {
    'use strict';
    var boardId = document.querySelector('.board-settings-page').dataset.boardId;
    var pageError = document.getElementById('numbering-error');
    function byId(id) { return document.getElementById(id); }
    function putNumbering(numbering, errorBox) {
      return fetch('/api/boards/' + encodeURIComponent(boardId) + '/config', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ numbering: numbering }),
      }).then(function (res) {
        if (res.ok) { window.location.reload(); return; }
        return res.json().catch(function () { return {}; }).then(function (body) {
          errorBox.textContent = body.message || 'Could not save the numbering.';
          errorBox.hidden = false;
        });
      });
    }
    function close(id) { byId(id).hidden = true; }

    var wizard = byId('numbering-wizard');
    if (byId('numbering-activate')) {
      var prefix = byId('numbering-wizard-prefix');
      var start = byId('numbering-wizard-start');
      var example = byId('numbering-wizard-example');
      var stepConfig = byId('numbering-step-config');
      var stepName = byId('numbering-step-name');
      var next = byId('numbering-wizard-next');
      var back = byId('numbering-wizard-back');
      var wizardError = byId('numbering-wizard-error');
      var onName = false;
      var showExample = function () {
        var first = parseInt(start.value, 10) || 1;
        var label = prefix.value.trim();
        example.textContent = 'Example: ' + label + '-' + first + ', ' + label + '-' + (first + 1);
      };
      var showStep = function (name) {
        onName = name;
        stepConfig.hidden = name;
        stepName.hidden = !name;
        back.hidden = !name;
        next.textContent = name ? 'Activate' : 'Next';
        wizardError.hidden = true;
      };
      prefix.addEventListener('input', showExample);
      start.addEventListener('input', showExample);
      byId('numbering-activate').addEventListener('click', function () {
        showExample();
        showStep(false);
        wizard.hidden = false;
        prefix.focus();
      });
      byId('numbering-wizard-cancel').addEventListener('click', function () { close('numbering-wizard'); });
      back.addEventListener('click', function () { showStep(false); });
      next.addEventListener('click', function () {
        if (!onName) { showStep(true); byId('numbering-wizard-name').focus(); return; }
        putNumbering({
          name: byId('numbering-wizard-name').value,
          prefix: prefix.value,
          start: parseInt(start.value, 10) || 1,
        }, wizardError);
      });
    }

    var fieldKey = null;
    var fieldInput = byId('numbering-field-input');
    var fieldError = byId('numbering-field-error');
    function openField(key, title, label, value) {
      fieldKey = key;
      byId('numbering-field-title').textContent = title;
      byId('numbering-field-label').textContent = label;
      fieldInput.value = value;
      fieldError.hidden = true;
      byId('numbering-field-dialog').hidden = false;
      fieldInput.focus();
    }
    if (byId('numbering-rename')) {
      byId('numbering-rename').addEventListener('click', function () {
        openField('name', 'Rename numbering', 'Name', byId('numbering-name').textContent);
      });
      byId('numbering-edit-prefix').addEventListener('click', function () {
        openField('prefix', 'Edit prefix', 'Prefix', byId('numbering-prefix').textContent);
      });
      byId('numbering-field-cancel').addEventListener('click', function () { close('numbering-field-dialog'); });
      byId('numbering-field-save').addEventListener('click', function () {
        var change = {};
        change[fieldKey] = fieldInput.value;
        putNumbering(change, fieldError);
      });
      byId('numbering-delete').addEventListener('click', function () { byId('numbering-delete-dialog').hidden = false; });
      byId('numbering-delete-cancel').addEventListener('click', function () { close('numbering-delete-dialog'); });
      byId('numbering-delete-confirm').addEventListener('click', function () {
        close('numbering-delete-dialog');
        putNumbering(null, pageError);
      });
    }
  };

  pages['board-settings-due-time'] = function () {
    'use strict';
    var dialog = document.getElementById('due-time-dialog');
    var input = document.getElementById('due-time-input');
    var error = document.getElementById('due-time-error');
    var label = document.getElementById('due-time-label');
    var boardId = document.querySelector('.board-settings-page').dataset.boardId;
    function close() { dialog.hidden = true; error.hidden = true; }
    document.getElementById('due-time-edit').addEventListener('click', function (e) {
      e.preventDefault();
      dialog.hidden = false;
      input.focus();
    });
    document.getElementById('due-time-cancel').addEventListener('click', close);
    document.getElementById('due-time-save').addEventListener('click', function () {
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/config', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ due_time: input.value }),
      }).then(function (res) {
        if (!res.ok) throw new Error('save failed');
        return res.json();
      }).then(function (config) {
        var parts = config.due_time.split(':');
        var h = Number(parts[0]);
        var account = document.getElementById('account-btn');
        label.textContent = account && account.dataset.timeFormat === '24h'
          ? parts[0] + ':' + parts[1]
          : ((h % 12) || 12) + ':' + parts[1] + ' ' + (h < 12 ? 'AM' : 'PM');
        close();
      }).catch(function () {
        error.textContent = 'Invalid time.';
        error.hidden = false;
      });
    });
  };

  pages['board-settings-default-member'] = function () {
    'use strict';
    var dialog = document.getElementById('default-member-dialog');
    var error = document.getElementById('default-member-error');
    var label = document.getElementById('default-member-label');
    var responsible = document.getElementById('default-member-responsible');
    var responsibleRow = document.getElementById('default-member-responsible-row');
    var boardId = document.querySelector('.board-settings-page').dataset.boardId;
    function chosen() { return dialog.querySelector('input[name="default-member"]:checked'); }
    function syncResponsible() {
      var pick = chosen();
      responsibleRow.hidden = !pick || pick.value === 'none';
    }
    function close() { dialog.hidden = true; error.hidden = true; }
    Array.prototype.forEach.call(dialog.querySelectorAll('input[name="default-member"]'), function (radio) {
      radio.addEventListener('change', syncResponsible);
    });
    syncResponsible();
    document.getElementById('default-member-edit').addEventListener('click', function (e) {
      e.preventDefault();
      dialog.hidden = false;
    });
    document.getElementById('default-member-cancel').addEventListener('click', close);
    document.getElementById('default-member-save').addEventListener('click', function () {
      var pick = chosen();
      if (!pick) return;
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/config', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ default_member: pick.value, default_member_responsible: responsible.checked }),
      }).then(function (res) {
        if (!res.ok) throw new Error('save failed');
        return res.json();
      }).then(function (config) {
        var text = config.default_member === 'none' ? 'None'
          : config.default_member === 'creator' ? 'Task creator'
          : chosen().parentNode.textContent.trim();
        label.textContent = text;
        close();
      }).catch(function () {
        error.textContent = 'Could not save the default member.';
        error.hidden = false;
      });
    });
  };

  pages['board-settings-advanced'] = function () {
    'use strict';
    var boardId = document.querySelector('.board-settings-page').dataset.boardId;
    var box = document.getElementById('bin-permanent-delete');
    var error = document.getElementById('bin-permanent-delete-error');
    box.addEventListener('change', function () {
      var wanted = box.checked;
      error.hidden = true;
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/config', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ bin_permanent_delete: wanted }),
      }).then(function (res) {
        if (!res.ok) throw new Error('save failed');
      }).catch(function () {
        box.checked = !wanted;
        error.textContent = 'Could not save the setting.';
        error.hidden = false;
      });
    });
  };

  pages['board-settings-fields'] = function () {
    var boardId = document.querySelector('.board-settings-page').dataset.boardId;
    var errorBox = document.getElementById('bf-error');
    function send(method, url, body) {
      return fetch(url, {
        method: method,
        credentials: 'same-origin',
        headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
        body: body === undefined ? undefined : JSON.stringify(body)
      }).then(function (res) {
        if (res.ok) return res;
        return res.text().then(function (text) {
          var message = text;
          try { message = JSON.parse(text).message || text; } catch (e) { /* plain text body */ }
          throw new Error(message || ('HTTP ' + res.status));
        });
      });
    }
    function showError(box, error) { box.textContent = error.message; box.hidden = false; }

    // One option per line, so an option may contain a comma.
    function splitOptions(text) {
      return text.split(/\r?\n/).map(function (s) { return s.trim(); }).filter(function (s) { return s; });
    }
    var COLUMN_KINDS = [['text', 'Text'], ['number', 'Number'], ['bool', 'Checkbox'],
      ['select', 'Select'], ['multiselect', 'Multiselect']];
    function hasOptions(kind) { return kind === 'select' || kind === 'multiselect'; }
    function columnField(text, control) {
      var label = document.createElement('label');
      label.appendChild(document.createTextNode(text));
      label.appendChild(control);
      return label;
    }
    function renumberColumns(box) {
      box.querySelectorAll('.bf-col').forEach(function (set, i) {
        set.querySelector('legend').textContent = 'Column ' + (i + 1);
        var remove = set.querySelector('.bf-col-remove');
        if (remove) remove.setAttribute('aria-label', 'Remove column ' + (i + 1));
      });
    }
    function addColumn(box, column, locked) {
      var set = document.createElement('fieldset');
      set.className = 'bf-col';
      set.appendChild(document.createElement('legend'));
      var key = document.createElement('input');
      key.type = 'text';
      key.name = 'column-key';
      key.value = column.key || '';
      key.maxLength = 64;
      key.placeholder = 'e.g. status';
      key.disabled = locked;
      var label = document.createElement('input');
      label.type = 'text';
      label.name = 'column-label';
      label.value = column.label || '';
      label.disabled = locked;
      var kind = document.createElement('select');
      kind.name = 'column-kind';
      COLUMN_KINDS.forEach(function (entry) {
        var option = document.createElement('option');
        option.value = entry[0];
        option.textContent = entry[1];
        kind.appendChild(option);
      });
      kind.value = column.kind || 'text';
      kind.disabled = locked;
      var options = document.createElement('textarea');
      options.name = 'column-options';
      options.rows = 3;
      options.placeholder = 'One per line';
      options.value = (column.options || []).join('\n');
      var optionsField = columnField('Options', options);
      var required = document.createElement('input');
      required.type = 'checkbox';
      required.name = 'column-required';
      required.checked = !!column.required;
      required.disabled = locked;
      var requiredField = columnField('', required);
      requiredField.className = 'check-label';
      requiredField.appendChild(document.createTextNode(' Required'));
      function syncOptions() { optionsField.hidden = !hasOptions(kind.value); }
      kind.addEventListener('change', syncOptions);
      syncOptions();
      set.appendChild(columnField('Key', key));
      set.appendChild(columnField('Label', label));
      set.appendChild(columnField('Kind', kind));
      set.appendChild(optionsField);
      set.appendChild(requiredField);
      if (!locked) {
        var remove = document.createElement('button');
        remove.type = 'button';
        remove.className = 'kf-btn kf-btn-danger bf-col-remove';
        remove.textContent = 'Remove';
        remove.addEventListener('click', function () {
          set.remove();
          renumberColumns(box);
        });
        set.appendChild(remove);
      }
      box.appendChild(set);
      renumberColumns(box);
    }
    function readColumns(box) {
      return Array.prototype.map.call(box.querySelectorAll('.bf-col'), function (set) {
        var column = {
          key: set.elements['column-key'].value.trim(),
          label: set.elements['column-label'].value,
          kind: set.elements['column-kind'].value,
          required: set.elements['column-required'].checked
        };
        if (hasOptions(column.kind)) column.options = splitOptions(set.elements['column-options'].value);
        return column;
      });
    }
    function wireAddColumn(buttonId, box) {
      document.getElementById(buttonId).addEventListener('click', function () {
        addColumn(box, {}, false);
      });
    }
    var addColumns = document.getElementById('bf-add-columns');
    wireAddColumn('bf-add-column', addColumns);
    document.querySelector('#bf-add select[name="kind"]').addEventListener('change', function (event) {
      var isTable = event.target.value === 'table';
      document.getElementById('bf-add-columns-row').hidden = !isTable;
      document.getElementById('bf-add-options-row').hidden = isTable;
      if (isTable && !addColumns.children.length) addColumn(addColumns, {}, false);
    });
    document.getElementById('bf-add').addEventListener('submit', function (event) {
      event.preventDefault();
      var form = event.target;
      var body = {
        label: form.label.value,
        key: form.key.value.trim(),
        kind: form.kind.value,
        owner: form.owner.value,
        required: form.required.checked
      };
      var options = splitOptions(form.options.value);
      if (body.kind === 'table') body.columns = readColumns(addColumns);
      else if (options.length) body.options = options;
      send('POST', '/api/boards/' + encodeURIComponent(boardId) + '/fields', body)
        .then(function () { window.location.reload(); })
        .catch(function (error) { showError(errorBox, error); });
    });

    // Styled in-page confirm instead of the browser dialog.
    var deleteOverlay = document.getElementById('bf-delete-overlay');
    var deleteId = null;
    document.querySelectorAll('.bf-delete').forEach(function (button) {
      button.addEventListener('click', function () {
        deleteId = button.dataset.fieldId;
        document.getElementById('bf-delete-name').textContent = button.dataset.label;
        deleteOverlay.hidden = false;
        document.getElementById('bf-delete-ok').focus();
      });
    });
    deleteOverlay.querySelectorAll('[data-close-dlg]').forEach(function (button) {
      button.addEventListener('click', function () { deleteOverlay.hidden = true; });
    });
    document.getElementById('bf-delete-ok').addEventListener('click', function () {
      deleteOverlay.hidden = true;
      send('DELETE', '/api/fields/' + encodeURIComponent(deleteId))
        .then(function () { window.location.reload(); })
        .catch(function (error) { showError(errorBox, error); });
    });
    var overlay = document.getElementById('bf-edit-overlay');
    var editId = null;
    var editKind = null;
    var editColumns = document.getElementById('bf-edit-columns');
    document.querySelectorAll('.bf-edit').forEach(function (button) {
      button.addEventListener('click', function () {
        editId = button.dataset.fieldId;
        editKind = button.dataset.kind;
        document.getElementById('bf-edit-label').value = button.dataset.label;
        editColumns.innerHTML = '';
        if (editKind === 'table') {
          JSON.parse(button.dataset.columns).forEach(function (column) {
            addColumn(editColumns, column, true);
          });
        }
        document.getElementById('bf-edit-columns-row').hidden = editKind !== 'table';
        document.getElementById('bf-edit-convert-row').hidden = editKind !== 'multiline';
        document.getElementById('bf-edit-options').value = button.dataset.options;
        document.getElementById('bf-edit-options-row').hidden = button.dataset.options === '';
        document.getElementById('bf-edit-required').checked = button.dataset.required === 'true';
        document.getElementById('bf-edit-error').hidden = true;
        overlay.hidden = false;
      });
    });
    overlay.querySelectorAll('[data-close-dlg]').forEach(function (button) {
      button.addEventListener('click', function () { overlay.hidden = true; });
    });
    document.getElementById('bf-edit-save').addEventListener('click', function () {
      var body = {
        label: document.getElementById('bf-edit-label').value,
        required: document.getElementById('bf-edit-required').checked
      };
      if (!document.getElementById('bf-edit-options-row').hidden) {
        body.options = splitOptions(document.getElementById('bf-edit-options').value);
      }
      if (editKind === 'table') body.columns = readColumns(editColumns);
      send('PATCH', '/api/fields/' + encodeURIComponent(editId), body)
        .then(function () { window.location.reload(); })
        .catch(function (error) { showError(document.getElementById('bf-edit-error'), error); });
    });
    wireAddColumn('bf-edit-add-column', editColumns);
    var convertOverlay = document.getElementById('bf-convert-overlay');
    var convertColumns = document.getElementById('bf-convert-columns');
    var convertKind = null;
    function openConvert(kind) {
      convertKind = kind;
      var name = document.getElementById('bf-edit-label').value;
      var isTable = kind === 'table';
      document.getElementById('bf-convert-title').textContent = 'Convert to ' + kind;
      document.getElementById('bf-convert-note').textContent = isTable
        ? 'Convert "' + name + '" to a table? This is one way and cannot be undone. Each task\'s lines become rows in the first text column, and nothing is deleted.'
        : 'Convert "' + name + '" to a multiselect? This is one way and cannot be undone. Each task\'s distinct lines become its selected options, and every line that is not an option yet is added to the options.';
      convertColumns.innerHTML = '';
      if (isTable) addColumn(convertColumns, { key: 'line', label: 'Line', kind: 'text' }, false);
      document.getElementById('bf-convert-columns-row').hidden = !isTable;
      document.getElementById('bf-convert-options-row').hidden = isTable;
      document.getElementById('bf-convert-options').value = '';
      document.getElementById('bf-convert-error').hidden = true;
      overlay.hidden = true;
      convertOverlay.hidden = false;
      document.getElementById('bf-convert-ok').focus();
    }
    document.getElementById('bf-convert-table').addEventListener('click', function () { openConvert('table'); });
    document.getElementById('bf-convert-multiselect').addEventListener('click', function () { openConvert('multiselect'); });
    wireAddColumn('bf-convert-add-column', convertColumns);
    convertOverlay.querySelectorAll('[data-close-dlg]').forEach(function (button) {
      button.addEventListener('click', function () { convertOverlay.hidden = true; });
    });
    document.getElementById('bf-convert-ok').addEventListener('click', function () {
      var body = { kind: convertKind };
      if (convertKind === 'table') {
        body.columns = readColumns(convertColumns);
      } else {
        var options = splitOptions(document.getElementById('bf-convert-options').value);
        if (options.length) body.options = options;
      }
      send('PATCH', '/api/fields/' + encodeURIComponent(editId), body)
        .then(function () { window.location.reload(); })
        .catch(function (error) { showError(document.getElementById('bf-convert-error'), error); });
    });
  };

  pages['board-settings-copy'] = function () {
    'use strict';
    var boardId = document.querySelector('.settings-page').getAttribute('data-board-id');
    function showError(msg) {
      var el = document.getElementById('bc-error');
      el.textContent = msg;
      el.hidden = false;
    }
    function clearError() {
      document.getElementById('bc-error').hidden = true;
    }
    function sendColor(method, colorId, data) {
      clearError();
      var url = '/api/boards/' + encodeURIComponent(boardId) + '/colors' +
        (colorId ? '/' + encodeURIComponent(colorId) : '');
      return fetch(url, {
        method: method,
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify(data),
      }).then(function (res) {
        if (res.ok) return { ok: true };
        return res.json().catch(function () { return {}; }).then(function (body) {
          return { ok: false, message: body.message || 'Could not save color changes.' };
        });
      });
    }
    function patchColor(colorId, data) {
      return sendColor('PATCH', colorId, data).then(function (result) {
        if (!result.ok) showError(result.message);
        return result.ok;
      });
    }

    var contrastOverlay = document.getElementById('bc-contrast-overlay');
    function saveHex(method, colorId, data, hexInput, errorBox) {
      return sendColor(method, colorId, data).then(function (result) {
        if (result.ok) { window.location.reload(); return; }
        var suggested = /low contrast[^]*(#[0-9a-f]{6})\s*$/i.exec(result.message);
        if (!suggested) {
          errorBox.textContent = result.message;
          errorBox.hidden = false;
          return;
        }
        document.getElementById('bc-contrast-message').textContent = result.message + '?';
        contrastOverlay.hidden = false;
        document.getElementById('bc-contrast-ok').onclick = function () {
          contrastOverlay.hidden = true;
          hexInput.value = suggested[1];
          hexInput.dispatchEvent(new Event('input'));
          data.hex = suggested[1];
          saveHex(method, colorId, data, hexInput, errorBox);
        };
        document.getElementById('bc-contrast-cancel').onclick = function () {
          contrastOverlay.hidden = true;
        };
      });
    }
    function linkPicker(picker, hex) {
      picker.addEventListener('input', function () { hex.value = picker.value; });
      hex.addEventListener('input', function () {
        if (/^#[0-9a-f]{6}$/i.test(hex.value)) picker.value = hex.value.toLowerCase();
      });
    }

    var customOverlay = document.getElementById('bc-custom-overlay');
    var customHex = document.getElementById('bc-custom-hex');
    var customError = document.getElementById('bc-custom-error');
    linkPicker(document.getElementById('bc-custom-picker'), customHex);
    document.getElementById('bc-add-custom').addEventListener('click', function () {
      customError.hidden = true;
      customOverlay.hidden = false;
    });
    document.getElementById('bc-custom-save').addEventListener('click', function () {
      saveHex('POST', '', {
        hex: customHex.value.trim(),
        label: document.getElementById('bc-custom-label').value,
        description: document.getElementById('bc-custom-description').value,
      }, customHex, customError);
    });

    // "Show more…" expander on the intro copy.
    var moreLink = document.getElementById('color-intro-more');
    var extra = document.getElementById('color-intro-extra');
    if (moreLink && extra) {
      moreLink.addEventListener('click', function (e) {
        e.preventDefault();
        var expanded = extra.hidden === false;
        extra.hidden = expanded;
        moreLink.textContent = expanded ? 'Show more...' : 'Show less';
      });
    }

    // Edit dialog: rename (up to 50 chars) and description; hex is fixed.
    var editOverlay = document.getElementById('bc-edit-overlay');
    var editLabel = document.getElementById('bc-edit-label');
    var editDesc = document.getElementById('bc-edit-description');
    var editError = document.getElementById('bc-edit-error');
    var editColorId = null;
    var editHexRow = document.getElementById('bc-edit-hex-row');
    var editHex = document.getElementById('bc-edit-hex');
    linkPicker(document.getElementById('bc-edit-picker'), editHex);
    function openEdit(colorId, label, description, hex) {
      editColorId = colorId;
      editLabel.value = label;
      editDesc.value = description;
      editHexRow.hidden = !hex;
      editHex.value = hex || '';
      editHex.dispatchEvent(new Event('input'));
      editError.hidden = true;
      editOverlay.hidden = false;
    }
    document.querySelectorAll('.bc-edit').forEach(function (btn) {
      btn.addEventListener('click', function () {
        openEdit(btn.getAttribute('data-color-id'),
          btn.getAttribute('data-label') || '',
          btn.getAttribute('data-description') || '',
          btn.getAttribute('data-hex'));
      });
    });
    document.getElementById('bc-edit-save').addEventListener('click', function () {
      if (!editColorId) return;
      if (editLabel.value.length > 50) {
        editError.textContent = 'Label must be 50 characters or fewer.';
        editError.hidden = false;
        return;
      }
      var data = { label: editLabel.value, description: editDesc.value };
      if (!editHexRow.hidden) data.hex = editHex.value.trim();
      saveHex('PATCH', editColorId, data, editHex, editError);
    });

    // Enable / Disable per row.
    document.querySelectorAll('.bc-disable').forEach(function (btn) {
      btn.addEventListener('click', function () {
        patchColor(btn.getAttribute('data-color-id'), { enabled: false }).then(function (ok) {
          if (ok) window.location.reload();
        });
      });
    });
    document.querySelectorAll('.bc-enable').forEach(function (btn) {
      btn.addEventListener('click', function () {
        patchColor(btn.getAttribute('data-color-id'), { enabled: true }).then(function (ok) {
          if (ok) window.location.reload();
        });
      });
    });

    // Drag-reorder enabled rows with SortableJS, which base.html loads deferred, so wire it
    // up on DOMContentLoaded.
    function initColorSort() {
      var list = document.getElementById('color-enabled-table');
      if (!list || !window.Sortable || Sortable.get(list.querySelector('tbody'))) return;
      new Sortable(list.querySelector('tbody'), {
        handle: '.color-drag',
        animation: 150,
        onEnd: function () {
          var rows = Array.prototype.slice.call(list.querySelectorAll('.color-admin-row'));
          var pending = [];
          rows.forEach(function (row, index) {
            var current = parseInt(row.getAttribute('data-sort-order'), 10);
            if (current !== index) {
              pending.push(patchColor(row.getAttribute('data-color-id'), { sort_order: index })
                .then(function (ok) { if (ok) row.setAttribute('data-sort-order', String(index)); }));
            }
          });
          Promise.all(pending).then(function () { /* order persisted; no reload needed */ });
        }
      });
    }
    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', initColorSort);
    else initColorSort();

    // Cancel, × and Close use app.js's shared [data-close-dialog]
    // handler; this only opens the dialog and closes it on a backdrop click.
    function wireDialog(openId, overlayId) {
      var openBtn = document.getElementById(openId);
      var overlay = document.getElementById(overlayId);
      if (!openBtn || !overlay) return;
      openBtn.addEventListener('click', function () { overlay.hidden = false; });
      overlay.addEventListener('click', function (e) {
        if (e.target === overlay) overlay.hidden = true;
      });
    }
    wireDialog('bc-set-default', 'bc-default-overlay');
    wireDialog('bc-copy-from', 'bc-copy-overlay');

    // Set default color.
    document.querySelectorAll('.bc-default-pick').forEach(function (btn) {
      btn.addEventListener('click', function () {
        patchColor(btn.getAttribute('data-color-id'), { is_default: true }).then(function (ok) {
          if (ok) window.location.reload();
        });
      });
    });

    // Copy from board.
    var copyConfirm = document.getElementById('bc-copy-confirm');
    if (copyConfirm) {
      copyConfirm.addEventListener('click', function () {
        var copyError = document.getElementById('bc-copy-error');
        var picked = document.querySelector('input[name="copy-source"]:checked');
        if (!picked) {
          copyError.textContent = 'Pick a board to copy the palette from.';
          copyError.hidden = false;
          return;
        }
        copyError.hidden = true;
        fetch('/api/boards/' + encodeURIComponent(boardId) + '/colors/copy-from', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          credentials: 'same-origin',
          body: JSON.stringify({ source_board_id: picked.value }),
        }).then(function (res) {
          if (!res.ok) {
            copyError.textContent = 'Could not copy the palette.';
            copyError.hidden = false;
            return;
          }
          window.location.reload();
        });
      });
    }
  };

  pages['board-settings-new-task-position'] = function () {
    'use strict';
    var boardId = document.querySelector('.settings-page').getAttribute('data-board-id');
    var overlay = document.getElementById('ntp-overlay');
    document.getElementById('ntp-edit').addEventListener('click', function (e) {
      var dlg = overlay.querySelector('.dlg');
      var r = e.currentTarget.getBoundingClientRect();
      overlay.hidden = false;
      dlg.style.left = Math.max(8, Math.min(r.left, window.innerWidth - dlg.offsetWidth - 8)) + 'px';
      dlg.style.top = Math.max(8, Math.min(r.bottom + 4, window.innerHeight - dlg.offsetHeight - 8)) + 'px';
    });
    overlay.addEventListener('click', function (e) {
      if (e.target === overlay || e.target.closest('[data-ntp-close]')) overlay.hidden = true;
    });
    document.getElementById('ntp-save').addEventListener('click', function () {
      var top = overlay.querySelector('input[name="ntp"]:checked').value === 'top';
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/config', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ new_tasks_at_top: top }),
      }).then(function (res) {
        if (!res.ok) throw new Error('save failed');
        window.location.reload();
      }).catch(function () {
        var err = document.getElementById('ntp-error');
        err.textContent = 'Could not save the position of new tasks.';
        err.hidden = false;
      });
    });
  };

  function webhookRequest(method, url, body) {
    return fetch(url, {
      method: method,
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body)
    }).then(function (res) {
      return res.text().then(function (text) {
        var data = null;
        try { data = text ? JSON.parse(text) : null; } catch (e) { data = null; }
        if (res.ok) return data;
        throw new Error((data && data.message) || text || ('HTTP ' + res.status));
      });
    });
  }

  function webhookProbeFailure(path, message) {
    if (message.indexOf('url must name a public host') !== 0) return message;
    return path === 'test-dns' ? 'Resolves to a non-public address' : 'refused: not a public address';
  }

  function webhookProbeText(result) {
    if (result.error) return result.error;
    return 'Answered with HTTP ' + result.status;
  }

  function wireWebhookSecretDialog() {
    var overlay = document.getElementById('wh-secret-overlay');
    var value = document.getElementById('wh-secret-value');
    var copy = document.getElementById('wh-secret-copy');
    copy.addEventListener('click', function () {
      value.select();
      if (!navigator.clipboard) return;
      navigator.clipboard.writeText(value.value).then(function () { copy.textContent = 'Copied'; });
    });
    return function (secret) {
      value.value = secret;
      copy.textContent = 'Copy';
      document.getElementById('wh-secret-error').hidden = true;
      overlay.hidden = false;
      value.focus();
      value.select();
      return overlay;
    };
  }

  function tokenRequest(method, id, body) {
    var send = method === 'GET' || method === 'DELETE' ? fetch : window.pombPasskey.fetch;
    return send('/api/v1/auth/tokens/' + encodeURIComponent(id), {
      method: method,
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body)
    }).then(function (res) {
      return res.text().then(function (text) {
        var data = null;
        try { data = text ? JSON.parse(text) : null; } catch (e) { data = null; }
        if (res.ok) return data;
        throw new Error((data && data.message) || text || ('HTTP ' + res.status));
      });
    });
  }

  function wireTokenRows() {
    if (!document.getElementById('token-table')) return;
    var errorBox = document.getElementById('tk-error');
    function fail(error) { errorBox.textContent = error.message; errorBox.hidden = false; }
    function overlay(id) { return document.getElementById(id); }
    var catalog = [];
    var presetsBox = document.getElementById('tk-edit-presets');
    var permsBox = document.getElementById('tk-edit-permissions');
    var editError = document.getElementById('tk-edit-error');
    var editId = null;
    var deleteId = null;
    function ticked() {
      return Array.prototype.map.call(permsBox.querySelectorAll('input:checked'), function (input) {
        return input.value;
      });
    }
    function option(type, name, value, text, title) {
      var label = document.createElement('label');
      label.className = 'check-label';
      var input = document.createElement('input');
      input.type = type;
      if (name) input.name = name;
      input.value = value;
      label.appendChild(input);
      label.appendChild(document.createTextNode(' ' + text));
      if (title) label.title = title;
      return label;
    }
    function renderCatalog(items) {
      catalog = items;
      presetsBox.textContent = '';
      TokenForm.presets(items).forEach(function (preset) {
        var label = option('radio', 'tk-preset', preset.name,
          preset.name + ' (' + preset.permissions.length + ' permissions)');
        label.querySelector('input').addEventListener('change', function () {
          permsBox.querySelectorAll('input').forEach(function (input) {
            input.checked = preset.permissions.indexOf(input.value) >= 0;
          });
        });
        presetsBox.appendChild(label);
      });
      permsBox.textContent = '';
      TokenForm.groups(items).forEach(function (group) {
        var set = document.createElement('fieldset');
        set.className = 'token-group';
        var legend = document.createElement('legend');
        legend.textContent = group.name;
        set.appendChild(legend);
        group.items.forEach(function (item) {
          var box = option('checkbox', '', item.name, item.name, item.description);
          box.querySelector('input').addEventListener('change', function () {
            presetsBox.querySelectorAll('input').forEach(function (radio) { radio.checked = false; });
          });
          set.appendChild(box);
        });
        permsBox.appendChild(set);
      });
    }
    var catalogLoaded = webhookRequest('GET', '/api/v1/auth/permissions').then(renderCatalog);
    catalogLoaded.catch(fail);
    function describe(data) {
      document.getElementById('tk-show-expiry').textContent = data.expires_at
        ? 'Expires ' + new Date(data.expires_at * 1000).toISOString().slice(0, 10)
        : 'Never expires';
      document.getElementById('tk-show-boards').textContent = data.boards === TokenForm.boardsAll
        ? 'Boards: all'
        : 'Boards: ' + data.boards.map(function (board) { return board.name; }).join(', ');
      var list = document.getElementById('tk-show-permissions');
      list.textContent = '';
      data.permissions.forEach(function (name) {
        var item = document.createElement('li');
        item.textContent = name;
        list.appendChild(item);
      });
    }
    document.querySelectorAll('.token-row').forEach(function (row) {
      var id = row.dataset.tokenId;
      row.querySelector('.tk-show').addEventListener('click', function () {
        errorBox.hidden = true;
        tokenRequest('GET', id).then(function (data) {
          document.getElementById('tk-show-name').textContent = data.name;
          document.getElementById('tk-show-value').textContent = data.prefix + '…' + data.last4;
          describe(data);
          overlay('tk-show-overlay').hidden = false;
        }).catch(fail);
      });
      row.querySelector('.tk-edit').addEventListener('click', function () {
        errorBox.hidden = true;
        Promise.all([tokenRequest('GET', id), catalogLoaded]).then(function (results) {
          var data = results[0];
          editId = id;
          document.getElementById('tk-edit-name').value = data.name;
          editError.hidden = true;
          presetsBox.querySelectorAll('input').forEach(function (radio) { radio.checked = false; });
          permsBox.querySelectorAll('input').forEach(function (input) {
            input.checked = data.permissions.indexOf(input.value) >= 0;
          });
          overlay('tk-edit-overlay').hidden = false;
          document.getElementById('tk-edit-name').focus();
        }).catch(fail);
      });
      row.querySelector('.tk-delete').addEventListener('click', function () {
        errorBox.hidden = true;
        deleteId = id;
        document.getElementById('tk-delete-name').textContent = row.dataset.name;
        overlay('tk-delete-overlay').hidden = false;
        document.getElementById('tk-delete-ok').focus();
      });
    });
    document.getElementById('tk-edit-form').addEventListener('submit', function (event) {
      event.preventDefault();
      editError.hidden = true;
      var request = TokenForm.body(catalog, {
        name: document.getElementById('tk-edit-name').value.trim(),
        ticked: ticked(),
        boards: null,
        expiryDays: null
      });
      if (!request.name) { editError.textContent = 'Give the token a name.'; editError.hidden = false; return; }
      if (!request.permissions.length) { editError.textContent = 'Tick at least one permission.'; editError.hidden = false; return; }
      tokenRequest('PATCH', editId, { name: request.name, permissions: request.permissions })
        .then(function () { window.location.reload(); })
        .catch(function (error) { editError.textContent = error.message; editError.hidden = false; });
    });
    document.getElementById('tk-delete-ok').addEventListener('click', function () {
      overlay('tk-delete-overlay').hidden = true;
      tokenRequest('DELETE', deleteId)
        .then(function () { window.location.reload(); })
        .catch(fail);
    });
  }

  pages['board-settings-webhooks'] = function () {
    wireTokenRows();
    var errorBox = document.getElementById('wh-error');
    function fail(error) { errorBox.textContent = error.message; errorBox.hidden = false; }
    var showSecret = wireWebhookSecretDialog();
    var openMenu = null;
    var deleteOverlay = document.getElementById('wh-delete-overlay');
    var deleteId = null;
    function closeMenu() {
      if (!openMenu) return;
      openMenu.menu.hidden = true;
      openMenu.button.setAttribute('aria-expanded', 'false');
      openMenu = null;
    }
    document.querySelectorAll('.webhook-row').forEach(function (row) {
      var id = row.dataset.webhookId;
      var menu = row.querySelector('.wh-menu');
      var button = row.querySelector('.wh-menu-btn');
      button.addEventListener('click', function (event) {
        event.stopPropagation();
        var wasOpen = openMenu && openMenu.menu === menu;
        closeMenu();
        if (wasOpen) return;
        var box = button.getBoundingClientRect();
        menu.style.top = box.bottom + 'px';
        menu.style.right = (window.innerWidth - box.right) + 'px';
        menu.hidden = false;
        button.setAttribute('aria-expanded', 'true');
        openMenu = { menu: menu, button: button };
      });
      row.querySelector('.wh-secret').addEventListener('click', function () {
        errorBox.hidden = true;
        webhookRequest('GET', '/api/webhooks/' + encodeURIComponent(id) + '/secret')
          .then(function (data) { showSecret(data.secret); })
          .catch(fail);
      });
      row.querySelector('.wh-toggle').addEventListener('click', function () {
        closeMenu();
        webhookRequest('PATCH', '/api/webhooks/' + encodeURIComponent(id), { enabled: row.dataset.enabled !== 'true' })
          .then(function () { window.location.reload(); })
          .catch(fail);
      });
      row.querySelector('.wh-delete').addEventListener('click', function () {
        closeMenu();
        deleteId = id;
        document.getElementById('wh-delete-name').textContent = row.dataset.name;
        deleteOverlay.hidden = false;
        document.getElementById('wh-delete-ok').focus();
      });
    });
    document.addEventListener('click', closeMenu);
    window.addEventListener('resize', closeMenu);
    window.addEventListener('scroll', closeMenu, true);
    document.getElementById('wh-delete-ok').addEventListener('click', function () {
      deleteOverlay.hidden = true;
      webhookRequest('DELETE', '/api/webhooks/' + encodeURIComponent(deleteId))
        .then(function () { window.location.reload(); })
        .catch(fail);
    });
  };

  pages['board-settings-inbound'] = () => {
    const errorBox = document.getElementById('ib-error');
    const fail = (error) => { errorBox.textContent = error.message; errorBox.hidden = false; };
    const overlay = document.getElementById('ib-address-overlay');
    const value = document.getElementById('ib-address-value');
    const copy = document.getElementById('ib-address-copy');
    const deleteOverlay = document.getElementById('ib-delete-overlay');
    let deleteId = null;
    copy.addEventListener('click', () => {
      value.select();
      if (!navigator.clipboard) return;
      navigator.clipboard.writeText(value.value).then(() => { copy.textContent = 'Copied'; });
    });
    overlay.addEventListener('click', (event) => {
      if (event.target.closest('[data-close-dialog]')) window.location.reload();
    });
    const showAddress = (issued) => {
      value.value = window.location.origin + issued.path;
      copy.textContent = 'Copy';
      overlay.hidden = false;
      value.focus();
      value.select();
    };
    const form = document.getElementById('ib-form');
    form.addEventListener('submit', (event) => {
      event.preventDefault();
      errorBox.hidden = true;
      const body = {
        name: document.getElementById('ib-new-name').value,
        prefix: document.getElementById('ib-new-prefix').value.trim(),
        column_id: document.getElementById('ib-new-column').value
      };
      webhookRequest('POST', `/api/boards/${encodeURIComponent(form.dataset.boardId)}/inbound-addresses`, body)
        .then(showAddress)
        .catch(fail);
    });
    document.querySelectorAll('.inbound-row').forEach((row) => {
      const id = row.dataset.inboundId;
      row.querySelector('.ib-rotate').addEventListener('click', () => {
        errorBox.hidden = true;
        webhookRequest('POST', `/api/inbound-addresses/${encodeURIComponent(id)}/rotate`)
          .then(showAddress)
          .catch(fail);
      });
      row.querySelector('.ib-delete').addEventListener('click', () => {
        deleteId = id;
        document.getElementById('ib-delete-name').textContent = row.dataset.name;
        deleteOverlay.hidden = false;
        document.getElementById('ib-delete-ok').focus();
      });
    });
    document.getElementById('ib-delete-ok').addEventListener('click', () => {
      deleteOverlay.hidden = true;
      webhookRequest('DELETE', `/api/inbound-addresses/${encodeURIComponent(deleteId)}`)
        .then(() => { window.location.reload(); })
        .catch(fail);
    });
  };

  pages['board-settings-webhook-form'] = function () {
    var form = document.getElementById('wh-form');
    var boardId = form.dataset.boardId;
    var webhookId = form.dataset.webhookId;
    var listUrl = '/b/' + encodeURIComponent(boardId) + '/settings/api-webhooks';
    var errorBox = document.getElementById('wh-form-error');
    var save = document.getElementById('wh-save');
    var showSecret = wireWebhookSecretDialog();

    var tabs = form.querySelectorAll('.wh-tab');
    function selectTab(name) {
      tabs.forEach(function (tab) {
        var on = tab.dataset.whTab === name;
        tab.setAttribute('aria-selected', on ? 'true' : 'false');
        tab.tabIndex = on ? 0 : -1;
        document.getElementById('wh-panel-' + tab.dataset.whTab).hidden = !on;
      });
    }
    tabs.forEach(function (tab) {
      tab.addEventListener('click', function () { selectTab(tab.dataset.whTab); });
    });

    function probe(button, result, path) {
      var url = document.getElementById('wh-url').value.trim();
      errorBox.hidden = true;
      result.textContent = 'Working...';
      button.disabled = true;
      webhookRequest('POST', '/api/boards/' + encodeURIComponent(boardId) + '/webhooks/' + path, { url: url })
        .then(function (data) { result.textContent = path === 'test-dns' ? data.message : webhookProbeText(data); })
        .catch(function (error) { result.textContent = webhookProbeFailure(path, error.message); })
        .then(function () { button.disabled = false; });
    }
    document.getElementById('wh-test-dns').addEventListener('click', function (event) {
      probe(event.target, document.getElementById('wh-dns-result'), 'test-dns');
    });
    document.getElementById('wh-test-request').addEventListener('click', function (event) {
      probe(event.target, document.getElementById('wh-test-result'), 'test-request');
    });

    document.getElementById('wh-secret-overlay').addEventListener('click', function (event) {
      if (event.target.closest('[data-close-dialog]')) window.location.assign(listUrl);
    });

    form.addEventListener('submit', function (event) {
      event.preventDefault();
      var body = {
        name: document.getElementById('wh-name').value,
        url: document.getElementById('wh-url').value.trim(),
        events: Array.prototype.map.call(form.querySelectorAll('input[name="event"]:checked'), function (box) { return box.value; }),
        column_id: document.getElementById('wh-column').value || null
      };
      errorBox.hidden = true;
      save.disabled = true;
      var request = webhookId
        ? webhookRequest('PATCH', '/api/webhooks/' + encodeURIComponent(webhookId), body)
        : webhookRequest('POST', '/api/boards/' + encodeURIComponent(boardId) + '/webhooks', body);
      request.then(function (data) {
        if (webhookId) { window.location.assign(listUrl); return; }
        showSecret(data.secret);
      }).catch(function (error) {
        errorBox.textContent = error.message;
        errorBox.hidden = false;
        save.disabled = false;
      });
    });
  };

  pages['board-settings-rules'] = function () {
    'use strict';
    var boardId = document.querySelector('.settings-page').getAttribute('data-board-id');
    var form = document.getElementById('br-form');
    var errorBox = document.getElementById('br-error');
    var savedBox = document.getElementById('br-saved');
    function lines(text) {
      return text.split(/\r?\n/).map(function (s) { return s.trim(); }).filter(function (s) { return s; });
    }
    function count(input) {
      return input.value.trim() === '' ? null : Number(input.value);
    }
    form.addEventListener('submit', function (event) {
      event.preventDefault();
      errorBox.hidden = true;
      savedBox.hidden = true;
      var columns = {};
      form.querySelectorAll('.br-column').forEach(function (box) {
        columns[box.dataset.columnId] = {
          sections: lines(box.querySelector('.br-sections').value),
          outcomes_min: count(box.querySelector('.br-min')),
          outcomes_max: count(box.querySelector('.br-max')),
          fields: Array.prototype.map.call(box.querySelectorAll('.br-field:checked'), function (input) { return input.value; })
        };
      });
      fetch('/api/boards/' + encodeURIComponent(boardId) + '/rules', {
        method: 'PUT',
        credentials: 'same-origin',
        headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
        body: JSON.stringify({ locked_labels: lines(document.getElementById('br-locked').value), columns: columns })
      }).then(function (res) {
        if (res.ok) { savedBox.hidden = false; return; }
        return res.text().then(function (text) {
          var message = text;
          try { message = JSON.parse(text).message || text; } catch (e) {}
          throw new Error(message || ('HTTP ' + res.status));
        });
      }).catch(function (error) {
        errorBox.textContent = error.message;
        errorBox.hidden = false;
      });
    });
  };

  pages['board-settings-labels'] = function () {
    'use strict';
    var section = document.getElementById('bl-section');
    var boardId = section.getAttribute('data-board-id');
    var body = document.getElementById('bl-rows');
    var errorBox = document.getElementById('bl-error');
    var savedBox = document.getElementById('bl-saved');
    var showAll = document.getElementById('bl-show-all');
    var palette = section.getAttribute('data-palette').split(',');
    function rows() {
      return Array.prototype.slice.call(body.querySelectorAll('.bl-row'));
    }
    var pending = Promise.resolve();
    function save() {
      pending = pending.then(send);
      return pending;
    }
    function send() {
      errorBox.hidden = true;
      savedBox.hidden = true;
      var highlights = [];
      var added = [];
      rows().forEach(function (row) {
        var label = row.dataset.label;
        if (row.querySelector('.bl-on').checked) {
          highlights.push({ label: label, color: row.querySelector('.bl-color').value });
        }
        if (row.dataset.cards === '0') added.push(label);
      });
      return fetch('/api/boards/' + encodeURIComponent(boardId) + '/label-settings', {
        method: 'PUT',
        credentials: 'same-origin',
        headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
        body: JSON.stringify({ highlights: highlights, show_all: showAll.checked, added: added })
      }).then(function (res) {
        if (res.ok) { savedBox.hidden = false; return; }
        return res.text().then(function (text) {
          var message = text;
          try { message = JSON.parse(text).message || text; } catch (e) {}
          throw new Error(message || ('HTTP ' + res.status));
        });
      }).catch(function (error) {
        errorBox.textContent = error.message + ' Reload the page to start from the saved labels.';
        errorBox.hidden = false;
      });
    }
    function build(label) {
      var row = document.createElement('tr');
      row.className = 'bl-row';
      row.dataset.label = label;
      row.dataset.cards = '0';
      var name = document.createElement('td');
      name.className = 'bl-name';
      name.textContent = label;
      var cards = document.createElement('td');
      cards.className = 'bl-cards';
      cards.textContent = '0';
      var cell = document.createElement('td');
      var highlight = document.createElement('div');
      highlight.className = 'bl-highlight';
      var on = document.createElement('input');
      on.type = 'checkbox';
      on.className = 'bl-on';
      on.setAttribute('aria-label', 'Highlight ' + label);
      var color = document.createElement('input');
      color.type = 'color';
      color.className = 'bl-color';
      color.value = palette[rows().length % palette.length];
      color.hidden = true;
      color.setAttribute('aria-label', label + ' color');
      highlight.appendChild(on);
      highlight.appendChild(color);
      cell.appendChild(highlight);
      var last = document.createElement('td');
      var remove = document.createElement('button');
      remove.type = 'button';
      remove.className = 'kf-btn kf-btn-link bl-remove';
      remove.textContent = 'Remove';
      remove.setAttribute('aria-label', 'Remove ' + label);
      last.appendChild(remove);
      row.appendChild(name);
      row.appendChild(cards);
      row.appendChild(cell);
      row.appendChild(last);
      return row;
    }
    body.addEventListener('change', function (event) {
      var row = event.target.closest('.bl-row');
      if (!row) return;
      if (event.target.classList.contains('bl-on')) {
        row.querySelector('.bl-color').hidden = !event.target.checked;
      }
      save();
    });
    body.addEventListener('click', function (event) {
      var button = event.target.closest('.bl-remove');
      if (!button) return;
      button.closest('.bl-row').remove();
      save();
    });
    showAll.addEventListener('change', save);
    document.getElementById('bl-add').addEventListener('submit', function (event) {
      event.preventDefault();
      var input = document.getElementById('bl-add-name');
      var label = input.value.trim();
      if (!label) return;
      var known = rows().some(function (row) { return row.dataset.label === label; });
      if (!known) body.appendChild(build(label));
      input.value = '';
      save();
    });
  };

  document.querySelectorAll('[data-page]').forEach(function (marker) {
    var page = pages[marker.dataset.page];
    if (typeof page !== 'function') {
      console.error('Unknown data-page: ' + marker.dataset.page);
      return;
    }
    page();
  });

  document.addEventListener('DOMContentLoaded', function () {
    document.querySelectorAll('[data-page-init]').forEach(function (marker) {
      var page = window[marker.dataset.pageInit];
      if (!page || typeof page.init !== 'function') {
        console.error('Unknown data-page-init: ' + marker.dataset.pageInit);
        return;
      }
      var args = [marker.dataset.boardId, marker.dataset.doneColumnId].filter(function (value) {
        return value !== undefined;
      });
      page.init.apply(page, args);
    });
  });
})();
