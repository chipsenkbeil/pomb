(function () {
  function fromBase64url(text) {
    var base64 = text.replace(/-/g, '+').replace(/_/g, '/');
    var binary = atob(base64 + '==='.slice((base64.length + 3) % 4));
    var bytes = new Uint8Array(binary.length);
    for (var i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes.buffer;
  }

  function toBase64url(buffer) {
    var bytes = new Uint8Array(buffer);
    var binary = '';
    for (var i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
    return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  }

  function decodeDescriptors(list) {
    (list || []).forEach(function (descriptor) { descriptor.id = fromBase64url(descriptor.id); });
  }

  function post(url, body) {
    return fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'Accept': 'application/json' },
      body: JSON.stringify(body || {})
    }).then(function (response) {
      return response.text().then(function (text) {
        var data = {};
        try { data = text ? JSON.parse(text) : {}; } catch (ignored) { data = { message: text }; }
        if (!response.ok) throw new Error(data.message || ('HTTP ' + response.status));
        return data;
      });
    });
  }

  function creationOptions(options) {
    var publicKey = options.publicKey;
    publicKey.challenge = fromBase64url(publicKey.challenge);
    publicKey.user.id = fromBase64url(publicKey.user.id);
    decodeDescriptors(publicKey.excludeCredentials);
    return { publicKey: publicKey };
  }

  function requestOptions(options) {
    var publicKey = options.publicKey;
    publicKey.challenge = fromBase64url(publicKey.challenge);
    decodeDescriptors(publicKey.allowCredentials);
    return { publicKey: publicKey };
  }

  function registration(credential) {
    var response = credential.response;
    return {
      id: credential.id,
      rawId: toBase64url(credential.rawId),
      type: credential.type,
      response: {
        clientDataJSON: toBase64url(response.clientDataJSON),
        attestationObject: toBase64url(response.attestationObject),
        transports: response.getTransports ? response.getTransports() : null
      },
      extensions: credential.getClientExtensionResults()
    };
  }

  function assertion(credential) {
    var response = credential.response;
    return {
      id: credential.id,
      rawId: toBase64url(credential.rawId),
      type: credential.type,
      response: {
        clientDataJSON: toBase64url(response.clientDataJSON),
        authenticatorData: toBase64url(response.authenticatorData),
        signature: toBase64url(response.signature),
        userHandle: response.userHandle ? toBase64url(response.userHandle) : null
      },
      extensions: credential.getClientExtensionResults()
    };
  }

  function supported() { return !!(window.PublicKeyCredential && navigator.credentials); }

  function showError(element, message) {
    element.textContent = message;
    element.hidden = false;
  }

  window.pombPasskey = {
    attempt: function (errorElement, prefix, action, needsWebAuthn) {
      errorElement.hidden = true;
      if (needsWebAuthn !== false && !supported()) {
        showError(errorElement, 'This browser does not support passkeys.');
        return Promise.resolve(false);
      }
      return Promise.resolve().then(action).then(function () { return true; }, function (ex) {
        showError(errorElement, prefix + ex.message);
        return false;
      });
    },

    register: function (optionsUrl, finishUrl, start, finish) {
      return post(optionsUrl, start).then(function (started) {
        return navigator.credentials.create(creationOptions(started.options)).then(function (credential) {
          var body = Object.assign({ ceremony: started.ceremony, credential: registration(credential) }, finish);
          return post(finishUrl, body);
        });
      });
    },

    finish: function (form, done) {
      if (!done.recovery_code) {
        location.assign(done.redirect);
        return;
      }
      document.getElementById('recovery-code-value').textContent = done.recovery_code;
      form.hidden = true;
      document.getElementById('recovery-code-panel').hidden = false;
      document.getElementById('recovery-code-continue').addEventListener('click', function () {
        location.assign(done.redirect);
      });
    },

    signIn: function () {
      return post('/api/v1/auth/sign-in/options').then(function (started) {
        return navigator.credentials.get(requestOptions(started.options)).then(function (credential) {
          return post('/api/v1/auth/sign-in', { ceremony: started.ceremony, credential: assertion(credential) });
        });
      });
    },

    remove: function (id) {
      return fetch('/api/v1/auth/passkeys/' + encodeURIComponent(id), {
        method: 'DELETE',
        headers: { 'Accept': 'application/json' }
      }).then(function (response) {
        if (response.ok) return response.json();
        return response.text().then(function (text) {
          var message = text;
          try { message = JSON.parse(text).message || text; } catch (ignored) { message = text; }
          throw new Error(message || ('HTTP ' + response.status));
        });
      });
    }
  };
})();
