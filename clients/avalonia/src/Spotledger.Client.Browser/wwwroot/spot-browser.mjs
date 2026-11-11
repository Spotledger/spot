// The page hooks of the Spotledger browser build (WP-UI-09.T1). `window.spotBrowser` exists as soon
// as this module is imported, before the .NET runtime has started, so a script can `await
// window.spotBrowser.ready` without any private knowledge of the build:
//
//   ready          Promise<void>; resolves when the shell (server picker, login or home) is interactive.
//   openForm(d,i)  Promise<boolean>; opens the form of doctype `d`, document `i` (after `ready`).
//   openList(d)    Promise<boolean>; opens the list of doctype `d` (after `ready`).
//   metrics()      string of JSON (see docs/browser-publish.md): marks in microseconds, counters, labels.
//
// This file is the only place those names are defined. It is loaded as a module twice by URL, from
// main.js and from the .NET side (JSHost.ImportAsync), and the browser gives both the same instance.

const state = {
  resolveReady: null,
  exports: null,
  jsMarks: { moduleLoaded: performance.now() },
};
const ready = new Promise((resolve) => { state.resolveReady = resolve; });

function dotnetMetrics() {
  if (!state.exports) return { schema: 1, ready: false, marksUs: {}, counters: {}, labels: {} };
  return JSON.parse(state.exports.MetricsJson());
}

const spotBrowser = {
  ready,
  async openForm(doctype, id) {
    await ready;
    return state.exports.OpenForm(String(doctype), id == null ? '' : String(id));
  },
  async openList(doctype) {
    await ready;
    return state.exports.OpenList(String(doctype));
  },
  metrics() {
    const metrics = dotnetMetrics();
    metrics.js = { marksMs: state.jsMarks };
    return JSON.stringify(metrics);
  },
};
Object.defineProperty(globalThis, 'spotBrowser', { value: Object.freeze(spotBrowser), enumerable: true });

// Called by main.js once the runtime has created the .NET exports.
export function attach(exports) { state.exports = exports; }
export function mark(name) { state.jsMarks[name] = performance.now(); }

// Called by the .NET side.
export function signalReady() { mark('ready'); state.resolveReady(); }
export function nowMicros() { return BigInt(Math.round(performance.now() * 1000)); }

// Browser storage for the .NET side. Every call is guarded: a browser may refuse storage.
export function storageAvailable() {
  try {
    const probe = 'spot.browser.probe';
    localStorage.setItem(probe, '1');
    localStorage.removeItem(probe);
    return true;
  } catch { return false; }
}
export function storageGet(key) { try { return localStorage.getItem(key); } catch { return null; } }
export function storageSet(key, value) { try { localStorage.setItem(key, value); } catch { /* refused: a convenience, not state */ } }
export function storageRemove(key) { try { localStorage.removeItem(key); } catch { /* refused */ } }
