// Runs the real spot-sw.js in a vm against a fake Cache API, fake fetch and fake clients: a cold start stores
// the files once, a warm start downloads nothing, the shell is served with the network down, a data request is
// never answered from the cache, and a file whose hash does not match is not stored. Run by SwPolicyNodeTests.
import { createHash, webcrypto } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import vm from 'node:vm';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const swDir = [join(here, '..', 'sw'), join(here, '..', '..', '..', 'src', 'Spotledger.Client.Browser', 'wwwroot', 'sw')]
  .find((d) => { try { readFileSync(join(d, 'spot-sw.js')); return true; } catch { return false; } });
const policySource = readFileSync(join(swDir, 'shell-policy.js'), 'utf8');
const workerSource = readFileSync(join(swDir, 'spot-sw.js'), 'utf8');

const ORIGIN = 'https://app.test';
const BASE = ORIGIN + '/app/';
const sha = (text) => createHash('sha256').update(text).digest('hex');

function makeServer(files) {
  const server = { files, hits: [], offline: false, corrupt: new Set() };
  const entries = Object.keys(files).sort().map((p) => ({ path: p, bytes: Buffer.byteLength(files[p]), sha256: sha(files[p]) }));
  const build = sha(entries.map((f) => f.path + '\u0000' + f.sha256 + '\n').join(''));
  server.manifest = { version: 1, build, files: entries };
  server.fetch = async (input) => {
    const url = typeof input === 'string' ? input : input.url;
    if (server.offline) throw new TypeError('offline');
    server.hits.push(url.slice(ORIGIN.length));
    const path = url.slice(BASE.length);
    if (path === 'assets.json') return new Response(JSON.stringify(server.manifest), { status: 200 });
    if (!(path in server.files)) return new Response('missing', { status: 404 });
    const text = server.corrupt.has(path) ? server.files[path] + 'X' : server.files[path];
    return new Response(text, { status: 200, headers: { 'Content-Type': 'text/plain' } });
  };
  return server;
}

function makeWorker(server, store) {
  const listeners = {};
  const sent = [];
  const cacheStore = store;
  const caches = {
    open: async () => ({
      match: async (url) => (cacheStore.has(url) ? cacheStore.get(url).clone() : undefined),
      put: async (url, response) => { cacheStore.set(typeof url === 'string' ? url : url.url, response); },
      delete: async (url) => cacheStore.delete(url),
      keys: async () => [...cacheStore.keys()].map((url) => ({ url })),
    }),
  };
  const self = {
    location: new URL(BASE + 'sw/spot-sw.js?contract=0.9.0'),
    addEventListener: (name, fn) => { listeners[name] = fn; },
    skipWaiting: () => {},
    clients: { claim: async () => {}, matchAll: async () => [{ postMessage: (m) => sent.push(m) }] },
    importScripts: undefined,
  };
  const context = vm.createContext({
    self, caches, fetch: server.fetch, crypto: webcrypto, Response, URL, TextEncoder, Promise, console,
    importScripts: () => { vm.runInContext(policySource, context); self.SpotShellPolicy = context.self.SpotShellPolicy; },
  });
  context.self = self;
  // The policy file attaches to the global it is given: here the worker's `self`.
  vm.runInContext(workerSource, context);
  const dispatchFetch = (url, init = {}) => new Promise((resolve, reject) => {
    const request = { url, method: init.method || 'GET', mode: init.mode || 'cors' };
    let handled = false;
    listeners.fetch({ request, respondWith: (p) => { handled = true; Promise.resolve(p).then(resolve, reject); } });
    if (!handled) resolve(null);
  });
  const activate = async () => { let done; listeners.activate({ waitUntil: (p) => { done = p; } }); await done; };
  return { sent, dispatchFetch, activate, listeners };
}

const files = { 'index.html': '<html>one</html>', 'main.js': 'one();', '_framework/a.wasm': 'wasm', '_framework/a.wasm.br': 'br' };
const server = makeServer(files);
const store = new Map();

// Cold start: everything cacheable is downloaded once.
let worker = makeWorker(server, store);
await worker.activate();
const cold = server.hits.filter((h) => !h.endsWith('assets.json'));
assert.deepEqual(cold.sort(), ['/app/_framework/a.wasm', '/app/index.html', '/app/main.js']);

// Offline: the shell is served from the cache, a data request is not.
server.offline = true;
let page = await worker.dispatchFetch(BASE + 'orders/42?x=1', { mode: 'navigate' });
assert.equal(await page.text(), '<html>one</html>');
let js = await worker.dispatchFetch(BASE + 'main.js');
assert.equal(await js.text(), 'one();');
assert.equal(await worker.dispatchFetch(ORIGIN + '/api/v1/doc/Order/1'), null, 'outside the app root: the worker does not answer');
// Even a data-looking URL under the app root is never answered from the cache: it goes to the network, which is down.
await assert.rejects(worker.dispatchFetch(BASE + 'api/v1/doc/Order/1'));
// A shell file asked with a query string is not a shell file.
await assert.rejects(worker.dispatchFetch(BASE + 'main.js?v=1'));
const dataKeys = [...store.keys()].filter((k) => k.includes('api/'));
assert.deepEqual(dataKeys, []);

// A second worker (the first was stopped) starts warm: it downloads nothing but the manifest.
server.offline = false;
server.hits.length = 0;
worker = makeWorker(server, store);
await worker.activate();
assert.deepEqual(server.hits, ['/app/assets.json']);

// A changed file downloads exactly that file; the running build keeps its files until the next navigation.
const next = makeServer({ ...files, 'main.js': 'two();' });
worker = makeWorker(next, store);
await worker.activate();
assert.deepEqual(next.hits.filter((h) => !h.endsWith('assets.json')), ['/app/main.js']);
assert.deepEqual(worker.sent.map((m) => m.event), ['update-found', 'update-ready']);
next.offline = true;
const sub = await worker.dispatchFetch(BASE + 'main.js');
assert.equal(await sub.text(), 'one();', 'a subresource is still served from the running build');
const nav = await worker.dispatchFetch(BASE, { mode: 'navigate' });
assert.equal(await nav.text(), '<html>one</html>');
const after = await worker.dispatchFetch(BASE + 'main.js');
assert.equal(await after.text(), 'two();', 'the next navigation activated the staged build');

// A file whose bytes do not match the manifest is not stored and nothing is activated.
const bad = makeServer({ ...files, 'main.js': 'three();' });
bad.corrupt.add('main.js');
const before = [...store.keys()].sort();
worker = makeWorker(bad, store);
await worker.activate();
assert.deepEqual([...store.keys()].sort(), before);
assert.equal(worker.sent.at(-1).event, 'update-failed');

console.log('ok');
