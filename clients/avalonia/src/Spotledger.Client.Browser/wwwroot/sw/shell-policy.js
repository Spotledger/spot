// WP-UI-09.T2: the shell policy as pure functions, no DOM, no Cache API, no fetch, no clock.
// This file is a line-for-line mirror of the C# library Spotledger.Client.Browser.Shell
// (ShellCachePlanner, OfflinePolicy, UpdateStateMachine, ShellManifest.IsSafePath). The service worker and the
// page bridge call these functions and decide nothing themselves. The golden vectors
// (tests/Spotledger.Client.Browser.Shell.Tests/Golden/vectors.json) run the same cases through both
// implementations, so a drift fails a test. Classic script: works with importScripts, a <script> tag and
// require() in Node.
(function (root) {
  'use strict';

  const HASH = /^[0-9a-f]{64}$/;

  function isHash(value) {
    return typeof value === 'string' && HASH.test(value);
  }

  // A plain relative path: no leading '/', no backslash, no '.' or '..' or empty segment, none of ? # :
  // and no control character.
  function isSafePath(path) {
    if (typeof path !== 'string' || path.length === 0 || path[0] === '/') return false;
    for (let i = 0; i < path.length; i++) {
      const c = path.charCodeAt(i);
      if (c < 0x20 || c === 0x7f || path[i] === '\\' || path[i] === '?' || path[i] === '#' || path[i] === ':') return false;
    }
    return path.split('/').every((s) => s.length > 0 && s !== '.' && s !== '..');
  }

  function cacheKey(entry) {
    return entry.path + '#' + entry.sha256;
  }

  function pathOfKey(key) {
    const cut = key.lastIndexOf('#');
    return cut < 0 ? key : key.slice(0, cut);
  }

  // The .br and .gz siblings are not cached: the browser decodes them and the plain file's hash is checked.
  function isCacheable(path) {
    return !path.endsWith('.br') && !path.endsWith('.gz');
  }

  function compareOrdinal(a, b) {
    return a < b ? -1 : a > b ? 1 : 0;
  }

  // Validates a parsed assets.json (an object). Returns { build, files } or throws an Error: fail closed.
  // The build id is checked by the caller with buildIdInput() and SHA-256 (async in the browser).
  function validateManifest(manifest) {
    if (!manifest || manifest.version !== 1) throw new Error('The asset manifest has an unknown version.');
    if (!isHash(manifest.build)) throw new Error('The asset manifest has no valid build id.');
    if (!Array.isArray(manifest.files)) throw new Error('The asset manifest has no file list.');
    let previous = null;
    for (const f of manifest.files) {
      if (!f || typeof f.path !== 'string' || !isHash(f.sha256) || !Number.isInteger(f.bytes) || f.bytes < 0) {
        throw new Error('The asset manifest has a malformed file entry.');
      }
      if (!isSafePath(f.path)) throw new Error('The asset manifest has an unsafe path.');
      if (previous !== null && compareOrdinal(previous, f.path) >= 0) {
        throw new Error('The asset manifest lists a path twice or out of order.');
      }
      previous = f.path;
    }
    return { build: manifest.build, files: manifest.files };
  }

  // The text whose SHA-256 is the build id (same recipe as the publish step).
  function buildIdInput(files) {
    return files.map((f) => f.path + '\u0000' + f.sha256 + '\n').join('');
  }

  // planCache: see ShellCachePlanner.Plan. record is { build, contract, keys } or null; presentKeys is an array
  // of cache keys; files is the new manifest's file list; returns { reason, fetch, keep, delete, clearAll }
  // where fetch, keep and delete are sorted cache keys.
  function planCache(record, presentKeys, nextBuild, files, contract) {
    const neededKeys = files.filter((f) => isCacheable(f.path)).map(cacheKey);
    const neededSet = new Set(neededKeys);
    const present = new Set(presentKeys);
    const sorted = (keys) => keys.slice().sort(compareOrdinal);
    const del = sorted([...present].filter((k) => !neededSet.has(k)));
    if (record === null) {
      return { reason: 'cold-start', fetch: sorted(neededKeys), keep: [], delete: del, clearAll: true };
    }
    if (record.contract !== contract) {
      return { reason: 'contract-changed', fetch: sorted(neededKeys), keep: [], delete: del, clearAll: true };
    }
    const fetch = sorted(neededKeys.filter((k) => !present.has(k)));
    const keep = sorted(neededKeys.filter((k) => present.has(k)));
    const reason = record.build === nextBuild ? (fetch.length === 0 ? 'unchanged' : 'repair') : 'update';
    return { reason, fetch, keep, delete: del, clearAll: false };
  }

  // classifyRequest: see OfflinePolicy.Decide. isShellPath is a function(path) -> boolean.
  function classifyRequest(method, relativePath, hasQuery, isNavigation, isShellPath) {
    if (String(method).toUpperCase() !== 'GET') return { strategy: 'network-only', cachePath: null };
    if (isNavigation) return { strategy: 'cache-first', cachePath: 'index.html' };
    if (!hasQuery && isSafePath(relativePath) && isShellPath(relativePath)) {
      return { strategy: 'cache-first', cachePath: relativePath };
    }
    return { strategy: 'network-only', cachePath: null };
  }

  // resolveServe: see OfflinePolicy.Resolve.
  function resolveServe(decision, cacheHasEntry, networkAvailable) {
    if (decision.strategy === 'cache-first' && cacheHasEntry) return 'cache';
    return networkAvailable ? 'network' : 'refuse';
  }

  // UpdateMachine: see UpdateStateMachine. onApplyStarted(build) is called when the state becomes 'applying'.
  class UpdateMachine {
    constructor(currentBuild, onApplyStarted) {
      this.currentBuild = currentBuild;
      this.pendingBuild = null;
      this.state = 'idle';
      this._editsKnown = false;
      this._hasUnsavedEdits = false;
      this._dismissed = false;
      this._applyRequested = false;
      this._onApplyStarted = onApplyStarted || function () {};
    }

    get editsBlockApply() {
      return !this._editsKnown || this._hasUnsavedEdits;
    }

    get promptVisible() {
      return this.state === 'update-ready' && !this._dismissed && !this.editsBlockApply;
    }

    onUpdateFound(build) {
      if (this.state === 'applying' || build === this.currentBuild || (this.state !== 'idle' && build === this.pendingBuild)) return;
      this.pendingBuild = build;
      this.state = 'update-found';
      this._dismissed = false;
      this._applyRequested = false;
    }

    onStaged(build) {
      if (this.state === 'update-found' && build === this.pendingBuild) this.state = 'update-ready';
    }

    onStageFailed(build) {
      if (this.state === 'update-found' && build === this.pendingBuild) {
        this.state = 'idle';
        this.pendingBuild = null;
      }
    }

    setUnsavedEdits(value) {
      this._editsKnown = true;
      this._hasUnsavedEdits = !!value;
      if (!value && this._applyRequested && this.state === 'update-ready') this._begin();
    }

    requestApply() {
      if (this.state !== 'update-ready') return 'not-ready';
      if (this.editsBlockApply) {
        this._applyRequested = true;
        return 'deferred';
      }
      this._begin();
      return 'started';
    }

    dismiss() {
      this._dismissed = true;
    }

    onActivated(build) {
      if (this.state === 'applying' && build === this.pendingBuild) {
        this.currentBuild = build;
        this.pendingBuild = null;
        this.state = 'idle';
        this._applyRequested = false;
        this._dismissed = false;
      }
    }

    _begin() {
      this._applyRequested = false;
      this.state = 'applying';
      this._onApplyStarted(this.pendingBuild);
    }
  }

  const api = {
    isHash, isSafePath, cacheKey, pathOfKey, isCacheable, validateManifest, buildIdInput,
    planCache, classifyRequest, resolveServe, UpdateMachine,
  };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.SpotShellPolicy = api;
})(typeof self !== 'undefined' ? self : globalThis);
