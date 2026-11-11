// Node test of the launch-arguments builder (WP-UI-09.T11).
// Run by `run.sh --self-test`; also runnable by hand: node tests/measure-launch-args.test.mjs
import assert from 'node:assert/strict';
import { buildLaunchArgs, summarizeLaunchFailure } from '../src/measure.mjs';

// The two flags a CI/self-hosted-runner container needs are always present, regardless of the
// GL backend requested: `--disable-dev-shm-usage` for a small/shared /dev/shm, and `--no-sandbox`
// for a host with no user-namespace or setuid-sandbox privilege (see the comment above
// `buildLaunchArgs` in src/measure.mjs for the evidence).
for (const gl of ['swiftshader', 'none', undefined]) {
  const args = buildLaunchArgs(gl);
  assert.ok(Array.isArray(args), `buildLaunchArgs(${gl}) must return an array`);
  assert.ok(args.includes('--no-sandbox'), `buildLaunchArgs(${gl}) must include --no-sandbox`);
  assert.ok(args.includes('--disable-dev-shm-usage'),
    `buildLaunchArgs(${gl}) must include --disable-dev-shm-usage`);
}

// The swiftshader-specific flags are added only when asked for, never otherwise.
const swiftshader = buildLaunchArgs('swiftshader');
for (const flag of ['--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist']) {
  assert.ok(swiftshader.includes(flag), `buildLaunchArgs('swiftshader') must include ${flag}`);
}

for (const gl of ['none', undefined]) {
  const args = buildLaunchArgs(gl);
  for (const flag of ['--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist']) {
    assert.ok(!args.includes(flag), `buildLaunchArgs(${gl}) must not include ${flag}`);
  }
}

// No duplicate flags, from any gl value.
for (const gl of ['swiftshader', 'none', undefined]) {
  const args = buildLaunchArgs(gl);
  assert.equal(args.length, new Set(args).size, `buildLaunchArgs(${gl}) must not repeat a flag`);
}

// summarizeLaunchFailure (WP-UI-09.T11): the console line must carry the native reason, not just
// Playwright's generic headline. Real shape from a failed CI run (42283982470, spot-laptop-2):
// a missing shared library the chrome-headless-shell binary needs.
const sampleCrash = [
  'browserType.launchPersistentContext: Target page, context or browser has been closed',
  'Browser logs:',
  '',
  '<launching> /path/to/chrome-headless-shell --headless --no-sandbox',
  '<launched> pid=12345',
  '[pid=12345][err] chrome-headless-shell: error while loading shared libraries: '
    + 'libatk-1.0.so.0: cannot open shared object file: No such file or directory',
  'Call log:',
  '  - <launching> /path/to/chrome-headless-shell --headless --no-sandbox',
].join('\n');
const summary = summarizeLaunchFailure(sampleCrash);
assert.ok(summary.startsWith('browserType.launchPersistentContext: Target page, context or browser has been closed'),
  'summarizeLaunchFailure must keep the headline');
assert.ok(summary.includes('libatk-1.0.so.0'),
  `summarizeLaunchFailure must surface the native [err] line, got: ${summary}`);
assert.ok(!summary.includes('Call log:'),
  'summarizeLaunchFailure must not repeat the noisy Call log section');

// No native [err] line (the generic close with no browser-process detail): the headline alone.
assert.equal(summarizeLaunchFailure('Target page, context or browser has been closed'),
  'Target page, context or browser has been closed');

console.log('ok');
