// The JavaScript half of the calculation module (WP-UI-09.T1). The module itself is optional: the
// shared Rust calculation compiled to WebAssembly (SPK-07) is published as calc/spot_calc.wasm when it
// exists and its SHA-256 matches assets.json (fail closed otherwise). When it is absent `init` resolves false and the .NET side keeps the managed backend, which
// gives the same results (the conformance corpus is the proof; see docs/browser-publish.md).
//
// Module ABI v1 (proposed in the publish document, not yet built; see its Open questions):
//   memory                         linear memory
//   spot_calc_abi() -> i32         1
//   spot_calc_alloc(len) -> ptr    reserve `len` bytes for the request (UTF-8 JSON)
//   spot_calc_eval(ptr, len) -> i32  evaluate; returns the byte length of the answer (UTF-8 JSON)
//   spot_calc_answer() -> ptr      where the answer of the last call starts
//   spot_calc_free(ptr, len)       release the request bytes
let exportsOfModule = null;
const encoder = new TextEncoder();
const decoder = new TextDecoder();

async function sha256Hex(bytes) {
  const digest = await crypto.subtle.digest('SHA-256', bytes);
  return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, '0')).join('');
}

// True only when `manifest` (the parsed assets.json) lists `path` with exactly the SHA-256 of `bytes`.
// Fails closed: a missing manifest, a missing entry or a differing hash is false.
export async function matchesManifest(bytes, manifest, path) {
  const entry = manifest && Array.isArray(manifest.files)
    ? manifest.files.find((f) => f && f.path === path)
    : null;
  if (!entry || typeof entry.sha256 !== 'string') return false;
  return (await sha256Hex(bytes)) === entry.sha256.toLowerCase();
}

// `url` is relative to this module; the manifest path is the same path relative to the web root,
// where assets.json lives next to this file.
export async function init(url) {
  try {
    const base = new URL(url, import.meta.url);
    const root = new URL('./', import.meta.url);
    const path = decodeURIComponent(base.pathname.slice(root.pathname.length));
    const manifestResponse = await fetch(new URL('assets.json', root));
    if (!manifestResponse.ok) return false;
    const manifest = await manifestResponse.json();
    const response = await fetch(base);
    if (!response.ok) return false;
    const bytes = await response.arrayBuffer();
    if (!(await matchesManifest(bytes, manifest, path))) return false;
    const { instance } = await WebAssembly.instantiate(bytes, {});
    if (instance.exports.spot_calc_abi?.() !== 1) return false;
    exportsOfModule = instance.exports;
    return true;
  } catch {
    return false;
  }
}

export function loaded() { return exportsOfModule !== null; }

export function evaluate(requestJson) {
  if (!exportsOfModule) throw new Error('calculation module not loaded');
  const bytes = encoder.encode(requestJson);
  const ptr = exportsOfModule.spot_calc_alloc(bytes.length);
  new Uint8Array(exportsOfModule.memory.buffer, ptr, bytes.length).set(bytes);
  const length = exportsOfModule.spot_calc_eval(ptr, bytes.length);
  exportsOfModule.spot_calc_free(ptr, bytes.length);
  return decoder.decode(new Uint8Array(exportsOfModule.memory.buffer, exportsOfModule.spot_calc_answer(), length));
}
