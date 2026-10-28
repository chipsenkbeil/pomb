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
          min_free_mib: limitValue('min_free_mib')
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
    function showError(msg) {
      var el = document.getElementById('nb-error');
      el.textContent = msg;
      el.hidden = false;
    }
    document.getElementById('nb-create').addEventListener('click', function () {
      var name = document.getElementById('nb-name').value.trim();
      if (!name) { showError('Board name is required.'); return; }

      // Count characters as the server does, not UTF-16 units.
      if (Array.from(name).length > 100) { showError('Board name must be 1-100 characters.'); return; }
      var picked = document.querySelector('input[name="template_id"]:checked');
      var templateId = picked && picked.value ? picked.value : null;
      fetch('/api/boards', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: name, template_id: templateId }),
      }).then(function (res) {
        if (!res.ok) { showError('Could not create the board.'); return null; }
        return res.json();
      }).then(function (data) {
        if (data && data.id) window.location.href = '/b/' + encodeURIComponent(data.id);
      }).catch(function () { showError('Could not create the board.'); });
    });
    document.getElementById('nb-name').addEventListener('keydown', function (e) {
      if (e.key === 'Enter') document.getElementById('nb-create').click();
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

  pages['settings'] = function () {
    var panes = document.querySelectorAll('.settings-pane');
    var navItems = document.querySelectorAll('.bshell-nav-item');
    function showPane() {
      var id = location.hash.slice(1);
      var known = false;
      panes.forEach(function (p) { if (p.id === id) known = true; });
      if (!known) id = 'timer';
      panes.forEach(function (p) { p.hidden = p.id !== id; });
      navItems.forEach(function (a) { a.classList.toggle('bshell-nav-active', a.dataset.pane === id); });
    }
    window.addEventListener('hashchange', showPane);
    showPane();
    var form = document.getElementById('token-create-form');
    var err = document.getElementById('token-error');
    function showError(msg) { err.textContent = msg; err.hidden = false; }
    form.addEventListener('submit', function (e) {
      e.preventDefault();
      err.hidden = true;
      var scopes = [document.querySelector('input[name="token-scope"]:checked').value];
      window.pombPasskey.fetch('/api/v1/auth/tokens', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          name: document.getElementById('token-name').value.trim(),
          scopes: scopes,
          expires_in_days: parseInt(document.getElementById('token-expiry').value, 10)
        })
      }).then(function (r) {
        if (!r.ok) return r.text().then(function (t) { throw new Error(t || ('HTTP ' + r.status)); });
        return r.json();
      }).then(function (data) {
        document.getElementById('token-created-value').textContent = data.raw_token;
        document.getElementById('token-created').hidden = false;
        form.reset();
        document.querySelector('input[name="token-scope"][value="work"]').checked = true;
        document.getElementById('token-expiry').value = '365';

        // Refresh the Active tokens list in place instead of a delayed full-page reload,
        // so the one-time secret above stays visible.
        refreshTokenList();
      }).catch(function (ex) { showError('Could not create token: ' + ex.message); });
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
    function bindTokenRevokeButtons(root) {
      root.querySelectorAll('.token-revoke').forEach(function (btn) {
        btn.addEventListener('click', function () {
          // Styled in-page confirmation instead of native confirm().
          var tokenId = btn.dataset.id;
          var tokenName = btn.dataset.name;
          document.getElementById('confirm-title').textContent = 'Revoke token';
          document.getElementById('confirm-message').textContent =
            'Revoke token "' + tokenName + '"? Anything using it will stop working.';
          var okBtn = document.getElementById('confirm-ok');
          okBtn.textContent = 'Revoke';

          // Replace the button to drop any previously attached listeners.
          var fresh = okBtn.cloneNode(true);
          okBtn.parentNode.replaceChild(fresh, okBtn);
          fresh.addEventListener('click', function () {
            document.getElementById('confirm-dialog').hidden = true;
            fetch('/api/v1/auth/tokens/' + encodeURIComponent(tokenId), { method: 'DELETE' })
              .then(function (r) {
                if (!r.ok) throw new Error('HTTP ' + r.status);
                location.reload();
              })
              .catch(function (ex) { showError('Could not revoke token: ' + ex.message); });
          });
          document.getElementById('confirm-dialog').hidden = false;
          fresh.focus();
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
            min_free_mib: parseInt(document.getElementById('storage-min-free').value, 10)
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
        label.textContent = ((h % 12) || 12) + ':' + parts[1] + ' ' + (h < 12 ? 'AM' : 'PM');
        close();
      }).catch(function () {
        error.textContent = 'Invalid time.';
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
    function patchColor(colorId, data) {
      clearError();
      return fetch('/api/boards/' + encodeURIComponent(boardId) + '/colors/' + encodeURIComponent(colorId), {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify(data),
      }).then(function (res) {
        if (!res.ok) { showError('Could not save color changes.'); return false; }
        return true;
      });
    }

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
    function openEdit(colorId, label, description) {
      editColorId = colorId;
      editLabel.value = label;
      editDesc.value = description;
      editError.hidden = true;
      editOverlay.hidden = false;
    }
    document.querySelectorAll('.bc-edit').forEach(function (btn) {
      btn.addEventListener('click', function () {
        openEdit(btn.getAttribute('data-color-id'),
          btn.getAttribute('data-label') || '',
          btn.getAttribute('data-description') || '');
      });
    });
    document.getElementById('bc-edit-save').addEventListener('click', function () {
      if (!editColorId) return;
      if (editLabel.value.length > 50) {
        editError.textContent = 'Label must be 50 characters or fewer.';
        editError.hidden = false;
        return;
      }
      patchColor(editColorId, { label: editLabel.value, description: editDesc.value })
        .then(function (ok) {
          if (ok) window.location.reload();
        });
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
    document.getElementById('ntp-edit').addEventListener('click', function () { overlay.hidden = false; });
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
