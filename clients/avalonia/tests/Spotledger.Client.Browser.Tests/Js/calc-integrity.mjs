// Runs from the test output folder (calc.mjs is copied one level up). Run by CalcIntegrityTests with node: exits non-zero on the first failed check.
import { createHash } from 'node:crypto';
import assert from 'node:assert/strict';
import { matchesManifest } from '../calc.mjs';

const bytes = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]);
const hash = createHash('sha256').update(bytes).digest('hex');
const manifest = (files) => ({ version: 1, files });
const path = 'calc/spot_calc.wasm';

assert.equal(await matchesManifest(bytes, manifest([{ path, sha256: hash }]), path), true);
assert.equal(await matchesManifest(bytes, manifest([{ path, sha256: hash.toUpperCase() }]), path), true);
assert.equal(await matchesManifest(bytes, manifest([{ path, sha256: '0'.repeat(64) }]), path), false);
assert.equal(await matchesManifest(bytes, manifest([{ path: 'other.wasm', sha256: hash }]), path), false);
assert.equal(await matchesManifest(bytes, manifest([{ path }]), path), false);
assert.equal(await matchesManifest(bytes, null, path), false);
assert.equal(await matchesManifest(bytes, {}, path), false);
const changed = new Uint8Array([...bytes, 1]);
assert.equal(await matchesManifest(changed, manifest([{ path, sha256: hash }]), path), false);
console.log('ok');
