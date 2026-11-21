// WP-UI-09.T2: the Spotledger shell service worker.
// It carries out the decisions of shell-policy.js with the Cache API and talks to the page by postMessage.
// It has no policy of its own: what to fetch, keep and delete, and what is served from the cache, come from
// SpotShellPolicy (a mirror of the C# library Spotledger.Client.Browser.Shell, checked by golden vectors).
//
// Rules this file keeps:
//  * only files listed in assets.json are stored, and only after their SHA-256 matched the manifest;
//  * a data response is never stored and never served from this cache (the worker only answers shell files
//    and page navigations itself; every other request goes to the network untouched);
//  * a new build is staged beside the running one and replaces it only on 'apply' or the next activation;
//  * any integrity failure stops the step and leaves the cache as it was (fail closed);
//  * nothing here reads or stores a session token or any other credential.
'use strict';

importScripts('shell-policy.js');

const P = self.SpotShellPolicy;
const CACHE = 'spot-shell-v1';
// The worker lives at <app root>/sw/spot-sw.js, so the app root is one level up.
const BASE = new URL('../', self.location.href).href;
// The contract version the page was built for; the page passes it in the worker URL (?contract=...).
const CONTRACT = new URL(self.location.href).searchParams.get('contract');
const FILE_PREFIX = BASE + '__spot/f/';
const RECORD_PREFIX = BASE + '__spot/r/';

let queue = Promise.resolve();
let staging = false;
let activating = null;
let activeRecord; // undefined = not loaded yet; null = none

function serialized(task) {
  const run = queue.then(task, task);
  queue = run.catch(() => {});
  return run;
}

function fileUrl(key) { return FILE_PREFIX + encodeURIComponent(key); }
function assetUrl(path) { return BASE + path.split('/').map(encodeURIComponent).join('/'); }

async function sha256Hex(buffer) {
  const digest = await crypto.subtle.digest('SHA-256', buffer);
  return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, '0')).join('');
}

async function readRecord(name) {
  const cache = await caches.open(CACHE);
  const response = await cache.match(RECORD_PREFIX + name);
  if (!response) return null;
  try {
    const record = await response.json();
    if (record && typeof record.build === 'string' && typeof record.contract === 'string' && Array.isArray(record.keys)) return record;
  } catch (_) { /* unreadable: treated as none */ }
  return null;
}

async function writeRecord(name, record) {
  const cache = await caches.open(CACHE);
  await cache.put(RECORD_PREFIX + name, new Response(JSON.stringify(record), { headers: { 'Content-Type': 'application/json' } }));
}

async function deleteRecord(name) {
  const cache = await caches.open(CACHE);
  await cache.delete(RECORD_PREFIX + name);
}

async function presentKeys() {
  const cache = await caches.open(CACHE);
  const requests = await cache.keys();
  return requests.map((r) => r.url).filter((u) => u.startsWith(FILE_PREFIX)).map((u) => decodeURIComponent(u.slice(FILE_PREFIX.length)));
}

async function broadcast(message) {
  const clients = await self.clients.matchAll({ includeUncontrolled: true });
  for (const client of clients) client.postMessage(Object.assign({ type: 'spot-shell' }, message));
}

async function loadManifest() {
  const response = await fetch(BASE + 'assets.json', { cache: 'no-store' });
  if (!response.ok) throw new Error('The asset manifest could not be downloaded.');
  const { build, files } = P.validateManifest(await response.json());
  const expected = await sha256Hex(new TextEncoder().encode(P.buildIdInput(files)));
  if (expected !== build) throw new Error('The asset manifest\'s build id does not match its files.');
  return { build, files };
}

// Downloads what the plan says, verified, then records the build as pending. Activates at once on a cold start.
async function stage(manifest) {
  if (!CONTRACT) throw new Error('The worker was registered without a contract version.');
  staging = true;
  try {
    if (activating) await activating;
    return await stageCore(manifest);
  } finally {
    staging = false;
  }
}

async function stageCore(manifest) {
  const active = await readRecord('active');
  const present = await presentKeys();
  const plan = P.planCache(active, present, manifest.build, manifest.files, CONTRACT);
  const cache = await caches.open(CACHE);
  const written = [];
  try {
    for (const key of plan.fetch) {
      const path = P.pathOfKey(key);
      const entry = manifest.files.find((f) => f.path === path);
      const response = await fetch(assetUrl(path), { cache: 'no-cache' });
      if (!response.ok) throw new Error('A shell file could not be downloaded.');
      const bytes = await response.arrayBuffer();
      if (bytes.byteLength !== entry.bytes || (await sha256Hex(bytes)) !== entry.sha256) {
        throw new Error('A downloaded shell file does not match the asset manifest.');
      }
      await cache.put(fileUrl(key), new Response(bytes, {
        headers: { 'Content-Type': response.headers.get('Content-Type') || 'application/octet-stream' },
      }));
      written.push(key);
    }
  } catch (error) {
    const keep = new Set([...(active ? active.keys : []), ...present]);
    for (const key of written) if (!keep.has(key)) await cache.delete(fileUrl(key));
    throw error;
  }
  const keys = manifest.files.filter((f) => P.isCacheable(f.path)).map(P.cacheKey).sort();
  await writeRecord('pending', { build: manifest.build, contract: CONTRACT, keys });
  if (active === null) await activate();
  return plan;
}

// Makes the pending build the active one and removes the keys it does not need.
async function activate() {
  const pending = await readRecord('pending');
  if (!pending) return null;
  const present = new Set(await presentKeys());
  if (pending.keys.some((k) => !present.has(k))) {
    await deleteRecord('pending'); // a file was evicted: do not activate a build that is not whole
    return null;
  }
  await writeRecord('active', pending);
  await deleteRecord('pending');
  activeRecord = pending;
  const needed = new Set(pending.keys);
  const cache = await caches.open(CACHE);
  for (const key of present) if (!needed.has(key)) await cache.delete(fileUrl(key));
  await broadcast({ event: 'activated', build: pending.build });
  return pending.build;
}

// A new page load activates a staged build first, so a new shell is used from the next load on. It is skipped
// while a download is running (the page then loads on the build it had).
function activateOnce() {
  if (!activating) activating = activate().catch(() => null).finally(() => { activating = null; });
  return activating;
}

async function check() {
  let build = null;
  try {
    const manifest = await loadManifest();
    build = manifest.build;
    const before = await readRecord('active');
    const isUpdate = before !== null && (before.build !== manifest.build || before.contract !== CONTRACT);
    if (isUpdate) await broadcast({ event: 'update-found', build });
    await stage(manifest);
    if (isUpdate) await broadcast({ event: 'update-ready', build });
    return { ok: true };
  } catch (error) {
    await broadcast({ event: 'update-failed', build, message: String((error && error.message) || error) });
    return { ok: false };
  }
}

async function currentActive() {
  if (activeRecord === undefined) activeRecord = await readRecord('active');
  return activeRecord;
}

function relativePathOf(url) {
  return url.href.startsWith(BASE) ? decodeURIComponent(url.pathname.slice(new URL(BASE).pathname.length)) : null;
}

async function serveShell(request, decision) {
  const active = await currentActive();
  const key = active && active.keys.find((k) => P.pathOfKey(k) === decision.cachePath);
  if (key) {
    const cache = await caches.open(CACHE);
    const cached = await cache.match(fileUrl(key));
    if (cached) return cached;
  }
  try {
    return await fetch(request);
  } catch (_) {
    return new Response('The application is offline and this file is not cached.', { status: 503, headers: { 'Content-Type': 'text/plain' } });
  }
}

self.addEventListener('install', () => { self.skipWaiting(); });

self.addEventListener('activate', (event) => {
  event.waitUntil(self.clients.claim().then(() => serialized(check)));
});

self.addEventListener('fetch', (event) => {
  const request = event.request;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  const relative = relativePathOf(url);
  const isNavigation = request.mode === 'navigate';
  if (relative === null && !isNavigation) return; // outside the app root: the network, untouched
  event.respondWith((async () => {
    if (isNavigation && !staging) await activateOnce();
    const active = await currentActive();
    if (!active) return fetch(request); // nothing installed yet
    const shellPaths = new Set(active.keys.map(P.pathOfKey));
    const decision = P.classifyRequest(request.method, relative === null ? '' : relative, url.search !== '', isNavigation, (p) => shellPaths.has(p));
    // Data and anything else: the network only. The cache is neither read nor written.
    if (decision.strategy !== 'cache-first') return fetch(request);
    return serveShell(request, decision);
  })());
});

self.addEventListener('message', (event) => {
  const data = event.data;
  if (!data || data.type !== 'spot-shell') return;
  if (data.cmd === 'check') event.waitUntil(serialized(check));
  else if (data.cmd === 'status') {
    event.waitUntil(currentActive().then((active) => {
      if (event.source) event.source.postMessage({ type: 'spot-shell', event: 'status', build: active ? active.build : null });
    }));
  }
});
