// Browser budget driver (WP-UI-09.T3).
//
// Serves the published browser build (or the built-in fixture) from a local
// static server, drives it through the `window.spotBrowser` hooks documented in
// clients/avalonia/docs/browser-publish.md, and writes one raw JSON file per
// run holding the samples for every requested budget id. `budget_cli.py
// aggregate` reduces those samples to the result files tools/ci/ui/perf_gate.py
// reads, so no arithmetic and no verdict lives here.
//
// usage: node src/measure.mjs --plan raw/plan.json --out raw [--runs N]
//                             [--root <wwwroot> | --fixture] [--url http://...]
//                             [--timeout-ms 180000] [--gl swiftshader|none]
//                             [--signed-in | --signed-out]
//
// --signed-in (WP-UI-09.T5) starts the loopback stub backend (stub-backend/),
// signs in through the shell's own server picker and login screen, and only
// then measures the three signed-in operations. The startup, download and
// memory measures stay signed-out and unchanged. Needs SPOT_BUDGETS_STUB=1 or
// CI=true (run.sh sets the former).
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { chromium } from 'playwright';
import { serve } from './serve.mjs';
import { startStub, TEST_USER, TEST_PASSWORD } from '../stub-backend/server.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const probePath = path.join(here, 'probe.js');
const fixtureRoot = path.resolve(here, '..', 'fixture');

// True when this file is run directly (`node src/measure.mjs ...`), false when another module
// (the launch-arguments builder test) imports it only for its pure functions. Keeps the driver
// runnable exactly as before while making `buildLaunchArgs` importable without a `--plan` file.
const isMain = Boolean(process.argv[1]) && import.meta.url === pathToFileURL(process.argv[1]).href;

const argv = process.argv.slice(2);
const option = (name, fallback) => {
  const index = argv.indexOf(`--${name}`);
  return index < 0 ? fallback : (argv[index + 1] ?? true);
};
const flag = (name) => argv.includes(`--${name}`);

let planPath, plan, fixture, root, outDir, runs, timeoutMs, gl, explicitUrl, signedIn;
if (isMain) {
  planPath = option('plan');
  if (!planPath) {
    console.error('measure: --plan <plan.json> is required (budget_cli.py plan --out ...)');
    process.exit(2);
  }
  plan = JSON.parse(fs.readFileSync(planPath, 'utf8'));
  fixture = flag('fixture');
  root = fixture ? fixtureRoot : option('root', path.resolve(here, '..', '..', '..', 'artifacts', 'browser-publish', 'wwwroot'));
  outDir = path.resolve(option('out', path.join(path.dirname(planPath), 'raw')));
  runs = Number(option('runs', plan.runs ?? 1));
  timeoutMs = Number(option('timeout-ms', 180000));
  gl = option('gl', 'swiftshader');
  explicitUrl = option('url', null);
  if (flag('signed-in') && flag('signed-out')) {
    console.error('measure: --signed-in and --signed-out are mutually exclusive');
    process.exit(2);
  }
  signedIn = flag('signed-in');
}

const DOCTYPE = 'Sales Invoice';
const DOCUMENT = 'INV-0001';
const LIST_DOCTYPE = 'Sales Invoice';
const KEY_SEQUENCE = ['7', '3', '9', '1', '.', '5', '2', '4', '8', '6', 'Backspace', '0', '1', '9', '5'];
const SCROLL_PRESSES = 120;
const TABS = 10;

// DevTools throttling. "fifty" is the brief's cold-start network, 20 ms latency
// assumed; "local" is no throttling for operation timings, as SPK-07 measured.
const PROFILES = {
  fifty: { offline: false, latency: 20, downloadThroughput: 50e6 / 8, uploadThroughput: 50e6 / 8 },
  fast3g: { offline: false, latency: 562.5, downloadThroughput: 1.6e6 / 8, uploadThroughput: 750e3 / 8 },
  local: { offline: false, latency: 0, downloadThroughput: -1, uploadThroughput: -1 },
};

const STARTUP = ['browser.download.compressed', 'browser.startup.cold', 'browser.startup.warm'];
const OPERATIONS = ['browser.form.open', 'browser.list.scroll.frame', 'browser.child-edit.key', 'browser.tabs.memory'];
const SIGNED_IN_OPERATIONS = ['browser.form.open', 'browser.list.scroll.frame', 'browser.child-edit.key'];

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Launch flags a CI/self-hosted-runner container needs (WP-UI-09.T11).
//
// `--disable-dev-shm-usage` was already here: Chromium's default `/dev/shm` can be too small (or
// shared) on a container/CI host, and that failure mode is well documented.
//
// `--no-sandbox` is kept for the same documented reason (a host with no user-namespace or
// setuid-sandbox privilege), but it is **not** this task's fix: a dispatched run on the real
// runner (`spot-laptop-2`, run 42283982470, WP-UI-09.T11's own branch) still failed with it
// present, and its raw run JSON shows Playwright had *already* added `--no-sandbox` itself before
// ours (two copies on the command line) — Playwright does that automatically when the caller is
// root, which this runner's job is. So the sandbox was never the problem.
//
// The real cause, found only by reading the full `error` field of that run's `budgets/raw/run-1.json`
// (the CI console only ever printed its first line, `summarizeLaunchFailure` below fixes that):
//
//   [pid=499359][err] .../chrome-headless-shell: error while loading shared libraries:
//   libatk-1.0.so.0: cannot open shared object file: No such file or directory
//
// `chrome-headless-shell` (the separate binary Playwright ships for `headless: true` since the
// "new headless" architecture) needs a system library this runner's image does not have. That
// matches every piece of circumstantial evidence this task started with exactly: a near-instant
// failure (`error while loading shared libraries` happens before the process can do anything, so
// before any crash log a sandbox problem would produce), and the fact that this never happened in
// `run.sh --self-test` run from this worktree (a different machine, with its libraries already
// present). This is a runner-image gap, not a Playwright flag: see `## Open questions` in the task
// report for the exact package to add to the CI runner image, which is outside
// `tools/ci/browser/`.
export function buildLaunchArgs(gl) {
  return [
    '--no-sandbox',
    '--disable-dev-shm-usage',
    '--enable-precise-memory-info',
    ...(gl === 'swiftshader'
      ? ['--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist']
      : []),
  ];
}

// Facts about the host's sandboxing support, gathered only after a launch failure, so a future
// run's raw JSON says why without guessing. Never throws: every probe is independently guarded.
function collectLaunchDiagnostics() {
  const diagnostics = {};
  try {
    diagnostics.freeMemBytes = os.freemem();
    diagnostics.totalMemBytes = os.totalmem();
  } catch { /* best effort */ }
  try {
    diagnostics.devShm = fs.statfsSync('/dev/shm');
  } catch (error) {
    diagnostics.devShmError = String(error.message || error);
  }
  try {
    diagnostics.unprivilegedUsernsClone =
      fs.readFileSync('/proc/sys/kernel/unprivileged_userns_clone', 'utf8').trim();
  } catch (error) {
    diagnostics.unprivilegedUsernsCloneError = String(error.message || error);
  }
  try {
    diagnostics.aptSandboxHelper = fs.existsSync('/usr/lib/chromium/chrome-sandbox')
      || fs.existsSync('/usr/lib/chromium-browser/chrome-sandbox');
  } catch { /* best effort */ }
  return diagnostics;
}

// Before WP-UI-09.T11, a launch failure's console line was `result.error.split('\n')[0]`: only
// the generic "Target page, context or browser has been closed" headline, never the one line that
// says why (Playwright puts the browser process's own stderr, like a missing shared library,
// several lines down in the same string, see `buildLaunchArgs` above). That is why this exact
// failure went undiagnosed for weeks of CI runs: the real cause was always in the uploaded
// `run-N.json` artifact, never in the console log anyone actually reads. This prints the headline
// plus every `[err]` line the browser itself produced, so the next failure is diagnosable from the
// job log alone. Exported so the node test can prove the exact case that bit this task.
export function summarizeLaunchFailure(message) {
  const lines = String(message).split('\n');
  const headline = lines[0] || '';
  const native = lines.filter((line) => line.includes('][err]'));
  return native.length ? `${headline} | ${native.join(' | ')}` : headline;
}

async function emulate(cdp, profileName) {
  const profile = PROFILES[profileName];
  if (!profile) throw new Error(`unknown network profile ${profileName}`);
  await cdp.send('Network.emulateNetworkConditions', profile);
}

async function waitForShell(page) {
  await page.waitForFunction(
    () => globalThis.spotBrowser && typeof globalThis.spotBrowser.metrics === 'function',
    null,
    { timeout: timeoutMs, polling: 50 },
  );
  await page.evaluate(async () => {
    const shell = globalThis.spotBrowser;
    if (shell.ready && typeof shell.ready.then === 'function') await shell.ready;
  });
  const raw = await page.evaluate(() => globalThis.spotBrowser.metrics());
  const metrics = typeof raw === 'string' ? JSON.parse(raw) : raw;
  if (!metrics || typeof metrics !== 'object') throw new Error('metrics() did not return an object');
  return metrics;
}

// One navigation measured to interactive: the page's own `js.marksMs.ready`
// mark (ms since its time origin) plus the bytes that crossed the wire.
async function measureLoad(page, wire, navigate) {
  wire.bytes = 0;
  wire.requests = 0;
  await navigate();
  const metrics = await waitForShell(page);
  const marks = (metrics.js && metrics.js.marksMs) || {};
  if (typeof marks.ready !== 'number' || !Number.isFinite(marks.ready)) {
    throw new Error('metrics().js.marksMs.ready is missing; the build must publish the ready mark');
  }
  return { readyMs: marks.ready, bytes: wire.bytes, requests: wire.requests, metrics };
}

async function openForm(page) {
  return page.evaluate(async ([doctype, id]) => {
    const started = performance.now();
    const opened = await globalThis.spotBrowser.openForm(doctype, id);
    await globalThis.__spotFrames(2);
    return opened ? performance.now() - started : -1;
  }, [DOCTYPE, DOCUMENT]);
}

async function openList(page) {
  return page.evaluate(async (doctype) => {
    const started = performance.now();
    const opened = await globalThis.spotBrowser.openList(doctype);
    await globalThis.__spotFrames(2);
    return opened ? performance.now() - started : -1;
  }, LIST_DOCTYPE);
}

async function clickSurface(page) {
  const viewport = page.viewportSize() || { width: 1280, height: 800 };
  await page.mouse.click(Math.round(viewport.width / 2), Math.round(viewport.height / 2));
}

async function scrollSamples(page) {
  await clickSurface(page);
  await page.evaluate(() => globalThis.__spotProbe.framesStart());
  for (let index = 0; index < SCROLL_PRESSES; index += 1) {
    await page.keyboard.press('PageDown');
    await sleep(16);
  }
  const intervals = await page.evaluate(() => globalThis.__spotProbe.framesStop());
  return intervals.filter((value) => Number.isFinite(value) && value > 0);
}

async function keySamples(page) {
  await clickSurface(page);
  await page.evaluate(() => globalThis.__spotProbe.keysStart());
  for (const key of KEY_SEQUENCE) {
    await page.keyboard.press(key);
    await sleep(30);
  }
  return page.evaluate(() => globalThis.__spotProbe.keysStop());
}

async function memorySample(page) {
  let opened = 0;
  for (let index = 0; index < TABS / 2; index += 1) {
    if (await openForm(page) > 0) opened += 1;
    if (await openList(page) > 0) opened += 1;
  }
  const measured = await page.evaluate(() => {
    const heap = (performance.memory && performance.memory.usedJSHeapSize) || 0;
    const wasm = globalThis.__spotProbe.wasmBytes();
    return { heap, wasm, total: heap + wasm };
  });
  return { ...measured, opened };
}

async function waitForStub(stub, what, predicate, ms = 20000) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (predicate(stub.stats())) return;
    await sleep(50);
  }
  throw new Error(`sign-in: the shell never ${what} (stub saw ${JSON.stringify(stub.stats())})`);
}

// Drives the real shell: server picker, then login, with the keyboard only. The
// shell paints into a canvas, so there is no DOM to query; the stub's own
// request counters are the evidence that each step happened. Tab order is that
// of ServerPickerView (url, Check, Next) and LoginView (user, password, remember
// me, Sign in), see clients/avalonia/src/Spotledger.Client.Shell/Views.
async function signInThroughShell(page, stub) {
  const before = stub.stats();
  await page.mouse.click(4, 4);
  await page.keyboard.press('Tab');
  await page.keyboard.type(stub.url, { delay: 5 });
  await page.keyboard.press('Tab');
  await page.keyboard.press('Space');
  await waitForStub(stub, 'checked the server', (s) => s.ready > before.ready && s.live > before.live);
  await sleep(300);
  await page.keyboard.press('Tab');
  await page.keyboard.press('Space');
  await sleep(500);
  await page.mouse.click(4, 4);
  await page.keyboard.press('Tab');
  await page.keyboard.type(TEST_USER, { delay: 5 });
  await page.keyboard.press('Tab');
  await page.keyboard.type(TEST_PASSWORD, { delay: 5 });
  await page.keyboard.press('Tab');
  await page.keyboard.press('Tab');
  await page.keyboard.press('Space');
  await waitForStub(stub, 'accepted the sign-in', (s) => s.logins > before.logins);
  await waitForStub(stub, 'loaded the session after sign-in', (s) => s.sessions > before.sessions);
}

// The fixture page has no sign-in screen (it is always "signed in"); the stub
// is still started and answers one real login over HTTP, so --self-test proves
// the stub and the driver's wiring without a .NET build.
async function signInFixture(stub) {
  const base = stub.url;
  const bad = await fetch(`${base}/api/v1/auth/login`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ user: TEST_USER, password: 'wrong' }),
  });
  if (bad.status !== 401) throw new Error(`the stub accepted a wrong password (${bad.status})`);
  const good = await fetch(`${base}/api/v1/auth/login`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ user: TEST_USER, password: TEST_PASSWORD }),
  });
  if (good.status !== 200) throw new Error(`the stub refused its own credential (${good.status})`);
}

async function ensureSignedIn(page, stub, run) {
  if (fixture) {
    await signInFixture(stub);
    return;
  }
  try {
    await signInThroughShell(page, stub);
  } catch (error) {
    const shot = path.join(outDir, `sign-in-failure-run-${run}.png`);
    await page.screenshot({ path: shot }).catch(() => {});
    throw new Error(`${error.message || error} (screenshot: ${shot})`);
  }
}

async function runMemory(page, result, wanted) {
  if (!wanted.has('browser.tabs.memory')) return;
  try {
    const memory = await memorySample(page);
    if (!(memory.total > 0)) throw new Error('the page reported no memory');
    result.measures['browser.tabs.memory'] = [memory.total];
    result.notes.memory = memory;
  } catch (error) {
    result.errors['browser.tabs.memory'] = String(error.message || error);
  }
}

async function runOnce(run, baseUrl, stub) {
  const wanted = new Set(plan.budgets.map((budget) => budget.id));
  const profileOf = new Map(plan.budgets.map((budget) => [budget.id, budget.profile || 'local']));
  const result = {
    run,
    fixture,
    startedAt: new Date().toISOString(),
    host: {
      cpuModel: os.cpus()[0]?.model || 'unknown',
      logicalCpus: os.cpus().length,
      memBytes: os.totalmem(),
      kernel: os.release(),
      node: process.version,
    },
    measures: {},
    errors: {},
    notes: {},
  };
  const launchArgs = buildLaunchArgs(gl);
  const profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'spot-budgets-profile-'));
  let context;
  try {
    context = await chromium.launchPersistentContext(profileDir, {
      headless: true,
      args: launchArgs,
      viewport: { width: 1280, height: 800 },
      deviceScaleFactor: 1,
    });
    await context.addInitScript({ path: probePath });
    const page = context.pages()[0] || await context.newPage();
    const cdp = await context.newCDPSession(page);
    await cdp.send('Network.enable');
    const wire = { bytes: 0, requests: 0 };
    cdp.on('Network.loadingFinished', (event) => {
      wire.bytes += event.encodedDataLength || 0;
      wire.requests += 1;
    });
    result.browser = `Chromium ${context.browser()?.version?.() ?? 'unknown'}`;

    const startup = STARTUP.filter((id) => wanted.has(id));
    if (startup.length) {
      const profiles = new Set(startup.map((id) => profileOf.get(id)));
      if (profiles.size !== 1) {
        throw new Error(`the startup measures must share one network profile, got ${[...profiles]}`);
      }
      const profileName = [...profiles][0];
      result.notes.profile = profileName;
      await emulate(cdp, profileName);
      let cold;
      try {
        cold = await measureLoad(page, wire, () => page.goto(baseUrl, { waitUntil: 'commit', timeout: timeoutMs }));
        if (wanted.has('browser.startup.cold')) result.measures['browser.startup.cold'] = [cold.readyMs];
        if (wanted.has('browser.download.compressed')) {
          if (!(cold.bytes > 0)) throw new Error('no bytes were counted on the cold load');
          result.measures['browser.download.compressed'] = [cold.bytes];
        }
        result.notes.cold = { bytes: cold.bytes, requests: cold.requests, readyMs: cold.readyMs };
      } catch (error) {
        result.errors['browser.startup.cold'] = String(error.message || error);
        result.errors['browser.download.compressed'] = String(error.message || error);
      }
      if (wanted.has('browser.startup.warm')) {
        try {
          const warm = await measureLoad(page, wire, () => page.reload({ waitUntil: 'commit', timeout: timeoutMs }));
          result.measures['browser.startup.warm'] = [warm.readyMs];
          result.notes.warm = { bytes: warm.bytes, requests: warm.requests, readyMs: warm.readyMs };
        } catch (error) {
          result.errors['browser.startup.warm'] = String(error.message || error);
        }
      }
    }

    const operations = OPERATIONS.filter((id) => wanted.has(id));
    if (operations.length) {
      await emulate(cdp, 'local');
      if (stub) {
        const needsPage = !STARTUP.some((id) => wanted.has(id));
        if (needsPage) {
          await page.goto(baseUrl, { waitUntil: 'commit', timeout: timeoutMs });
          await waitForShell(page);
        }
        // Memory stays signed-out, as before this task: measure it first.
        await runMemory(page, result, wanted);
        if (SIGNED_IN_OPERATIONS.some((id) => wanted.has(id))) {
          try {
            await ensureSignedIn(page, stub, run);
            result.notes.signedIn = true;
          } catch (error) {
            for (const id of SIGNED_IN_OPERATIONS.filter((op) => wanted.has(op))) {
              result.errors[id] = String(error.message || error);
            }
          }
        }
      }
      const forms = [];
      if (wanted.has('browser.form.open') && !result.errors['browser.form.open']) {
        try {
          for (let index = 0; index < 3; index += 1) {
            const sample = await openForm(page);
            if (sample > 0) forms.push(sample);
          }
          if (!forms.length) throw new Error('openForm returned false: the shell cannot open a form (not signed in?)');
          result.measures['browser.form.open'] = forms;
        } catch (error) {
          result.errors['browser.form.open'] = String(error.message || error);
        }
      }
      if (wanted.has('browser.list.scroll.frame') && !result.errors['browser.list.scroll.frame']) {
        try {
          if (!(await openList(page) > 0)) throw new Error('openList returned false: the shell cannot open a list');
          const intervals = await scrollSamples(page);
          if (!intervals.length) throw new Error('no animation frames were observed while scrolling');
          result.measures['browser.list.scroll.frame'] = intervals;
          result.notes.scrollFrames = intervals.length;
        } catch (error) {
          result.errors['browser.list.scroll.frame'] = String(error.message || error);
        }
      }
      if (wanted.has('browser.child-edit.key') && !result.errors['browser.child-edit.key']) {
        try {
          if (!(await openForm(page) > 0)) throw new Error('openForm returned false: no form to type into');
          const latencies = await keySamples(page);
          if (!latencies.length) throw new Error('no key latency sample was collected');
          result.measures['browser.child-edit.key'] = latencies;
        } catch (error) {
          result.errors['browser.child-edit.key'] = String(error.message || error);
        }
      }
      if (!stub) await runMemory(page, result, wanted);
    }
  } catch (error) {
    result.error = String((error && error.stack) || error);
    result.diagnostics = collectLaunchDiagnostics();
  } finally {
    if (context) await context.close().catch(() => {});
    fs.rmSync(profileDir, { recursive: true, force: true });
  }
  return result;
}

if (isMain) {
  fs.mkdirSync(outDir, { recursive: true });
  const server = explicitUrl ? null : await serve(root);
  const baseUrl = explicitUrl || `${server.url}/`;
  // The stub allows the page's origin and no other (no wildcard CORS).
  const stub = signedIn ? await startStub({ allowOrigins: [new URL(baseUrl).origin] }) : null;
  let failures = 0;
  try {
    for (let run = 1; run <= runs; run += 1) {
      const result = await runOnce(run, baseUrl, stub);
      const file = path.join(outDir, `run-${run}.json`);
      fs.writeFileSync(file, JSON.stringify(result, null, 1) + '\n');
      const summary = Object.entries(result.measures)
        .map(([id, samples]) => `${id}=${samples.length} sample(s)`).join(' ');
      const errors = Object.keys(result.errors);
      console.log(`${file}: ${summary}${errors.length ? ` errors: ${errors.join(',')}` : ''}`);
      if (result.error) {
        failures += 1;
        console.error(`run ${run} failed: ${summarizeLaunchFailure(result.error)}`);
      }
      if (errors.length) console.error(`run ${run}: ${errors.map((id) => `${id}: ${result.errors[id]}`).join('; ')}`);
    }
  } finally {
    if (stub) await stub.close();
    if (server) await server.close();
  }
  process.exit(failures ? 1 : 0);
}
