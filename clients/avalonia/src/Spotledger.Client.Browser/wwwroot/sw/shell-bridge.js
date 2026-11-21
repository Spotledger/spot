// WP-UI-09.T2: the page side of the shell cache. It registers the service worker, relays its messages to the
// update state machine of shell-policy.js and shows the "update ready" prompt. It decides nothing itself.
//
//  * The prompt is a visible, dismissible banner. It is shown only while the app reports that it has no
//    unsaved edits: window.spotShell.setUnsavedEdits(true|false) is the hook. Until the app has called it once,
//    edits are assumed to exist (fail closed), so an app that does not report simply gets the new build on its
//    next load and never sees a prompt.
//  * "Update now" reloads the page; the worker activates the staged build on that navigation.
//  * Nothing is written to localStorage, sessionStorage or cookies, and no token is read.
(function () {
  'use strict';
  const P = self.SpotShellPolicy;
  if (!P || !('serviceWorker' in navigator)) return;
  const meta = document.querySelector('meta[name="spot-contract"]');
  const contract = meta && meta.getAttribute('content');
  if (!contract) return; // no contract version: do not cache (fail closed)

  const machine = new P.UpdateMachine(null, function () { location.reload(); });
  let banner = null;

  function render() {
    const visible = machine.promptVisible;
    if (!visible) {
      if (banner) banner.hidden = true;
      return;
    }
    if (!banner) {
      banner = document.createElement('div');
      banner.setAttribute('role', 'status');
      banner.style.cssText = 'position:fixed;left:50%;bottom:16px;transform:translateX(-50%);z-index:2147483647;' +
        'display:flex;gap:12px;align-items:center;padding:10px 16px;border-radius:8px;background:#1f2937;color:#fff;' +
        'font:14px system-ui,sans-serif;box-shadow:0 4px 16px rgba(0,0,0,.3)';
      const text = document.createElement('span');
      text.textContent = 'A new version of Spotledger is ready.';
      const now = document.createElement('button');
      now.type = 'button';
      now.textContent = 'Update now';
      now.addEventListener('click', function () { machine.requestApply(); render(); });
      const later = document.createElement('button');
      later.type = 'button';
      later.textContent = 'Later';
      later.addEventListener('click', function () { machine.dismiss(); render(); });
      banner.append(text, now, later);
      document.body.appendChild(banner);
    }
    banner.hidden = false;
  }

  function post(cmd) {
    const target = navigator.serviceWorker.controller;
    if (target) target.postMessage({ type: 'spot-shell', cmd: cmd });
  }

  navigator.serviceWorker.addEventListener('message', function (event) {
    const m = event.data;
    if (!m || m.type !== 'spot-shell') return;
    if (m.event === 'status') { if (machine.currentBuild === null) machine.currentBuild = m.build; }
    else if (m.event === 'update-found') machine.onUpdateFound(m.build);
    else if (m.event === 'update-ready') machine.onStaged(m.build);
    else if (m.event === 'update-failed') { if (m.build) machine.onStageFailed(m.build); }
    render();
  });

  window.spotShell = {
    setUnsavedEdits: function (value) { machine.setUnsavedEdits(value); render(); },
    checkForUpdate: function () { post('check'); },
    get state() { return machine.state; },
  };

  navigator.serviceWorker
    .register('sw/spot-sw.js?contract=' + encodeURIComponent(contract), { scope: './' })
    .then(function () { return navigator.serviceWorker.ready; })
    .then(function () { post('status'); post('check'); })
    .catch(function () { /* the browser refused (private window, missing scope header): run without a cache */ });

  document.addEventListener('visibilitychange', function () {
    if (document.visibilityState === 'visible') post('check');
  });
})();
