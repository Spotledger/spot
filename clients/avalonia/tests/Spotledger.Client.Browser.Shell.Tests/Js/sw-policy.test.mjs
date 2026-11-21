// Node test of the service worker's pure policy file against the golden vectors shared with the C# tests.
// Run by SwPolicyNodeTests when node is installed; also runnable by hand: node sw-policy.test.mjs
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
// Layout 1 (test output): ../sw/shell-policy.js and ../Golden/vectors.json relative to Js/.
// Layout 2 (source tree): the worker's folder and the Golden folder of the test project.
const require = createRequire(import.meta.url);
const policyCandidates = [join(here, '..', 'sw', 'shell-policy.js'), join(here, '..', '..', '..', 'src', 'Spotledger.Client.Browser', 'wwwroot', 'sw', 'shell-policy.js')];
const vectorCandidates = [join(here, '..', 'Golden', 'vectors.json'), join(here, 'vectors.json'), join(here, '..', 'Golden', 'vectors.json')];
const pick = (list) => list.find((p) => { try { readFileSync(p); return true; } catch { return false; } });
const P = require(pick(policyCandidates));
const vectors = JSON.parse(readFileSync(pick(vectorCandidates), 'utf8'));

const toFiles = (list) => list.map((s) => { const i = s.lastIndexOf(':'); return { path: s.slice(0, i), sha256: s.slice(i + 1) }; });

for (const v of vectors.planner) {
  const record = v.record === null ? null : { build: v.record.build, contract: v.record.contract, keys: [] };
  const plan = P.planCache(record, v.present, v.next.build, toFiles(v.next.files), v.contract);
  assert.deepEqual(plan, v.expect, v.name);
  const needed = new Set(toFiles(v.next.files).filter((f) => P.isCacheable(f.path)).map(P.cacheKey));
  assert.ok(plan.delete.every((k) => !needed.has(k)), v.name + ': deleted a needed key');
}

for (const v of vectors.classify) {
  const shell = new Set(v.shell);
  const d = P.classifyRequest(v.method, v.path, v.query, v.navigation, (p) => shell.has(p));
  assert.deepEqual(d, v.expect, v.name);
}

for (const v of vectors.resolve) {
  const decision = { strategy: v.strategy, cachePath: v.strategy === 'cache-first' ? 'x' : null };
  assert.equal(P.resolveServe(decision, v.cached, v.network), v.expect, JSON.stringify(v));
}

for (const v of vectors.update) {
  const started = [];
  const m = new P.UpdateMachine(v.current, (b) => started.push(b));
  for (const step of v.steps) {
    let result;
    switch (step.do) {
      case 'edits': m.setUnsavedEdits(step.value); break;
      case 'found': m.onUpdateFound(step.build); break;
      case 'staged': m.onStaged(step.build); break;
      case 'failed': m.onStageFailed(step.build); break;
      case 'apply': result = m.requestApply(); break;
      case 'dismiss': m.dismiss(); break;
      case 'activated': m.onActivated(step.build); break;
      default: assert.fail('unknown step ' + step.do);
    }
    assert.equal(m.state, step.expect.state, v.name + ' / ' + step.do + ': state');
    assert.equal(m.promptVisible, step.expect.prompt, v.name + ' / ' + step.do + ': prompt');
    if ('result' in step.expect) assert.equal(result, step.expect.result, v.name + ' / ' + step.do + ': result');
  }
}

// Manifest validation fails closed.
const h = 'a'.repeat(64);
const good = { version: 1, build: h, files: [{ path: 'a.js', bytes: 1, sha256: h }, { path: 'b.js', bytes: 1, sha256: h }] };
assert.equal(P.validateManifest(good).build, h);
const bad = (m) => assert.throws(() => P.validateManifest(m));
bad(null);
bad({ ...good, version: 2 });
bad({ ...good, build: 'xyz' });
bad({ ...good, files: 'no' });
bad({ ...good, files: [{ path: '../x', bytes: 1, sha256: h }] });
bad({ ...good, files: [{ path: '/x', bytes: 1, sha256: h }] });
bad({ ...good, files: [{ path: 'a.js', bytes: 1, sha256: 'A'.repeat(64) }] });
bad({ ...good, files: [{ path: 'a.js', bytes: -1, sha256: h }] });
bad({ ...good, files: [{ path: 'b.js', bytes: 1, sha256: h }, { path: 'a.js', bytes: 1, sha256: h }] });
bad({ ...good, files: [{ path: 'a.js', bytes: 1, sha256: h }, { path: 'a.js', bytes: 1, sha256: h }] });
assert.equal(P.isSafePath('a/b.js'), true);
for (const p of ['', 'a//b', './a', 'a/..', 'a\\b', 'a?b', 'a#b', 'c:d', 'a\nb']) assert.equal(P.isSafePath(p), false, JSON.stringify(p));
console.log('ok');
