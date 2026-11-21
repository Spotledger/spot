// Capture the published browser head for visual regression (WP-UI-16.T2).
//
// Drives the documented page contract only (`window.spotBrowser.ready/openForm/openList/metrics`,
// clients/avalonia/docs/browser-publish.md) at the fixed viewports in scenarios.json, waits for the
// app-ready signal, disables animations and the caret, and writes one PNG per (scenario, viewport)
// plus a manifest. A scenario the build cannot reach is written to the manifest as unavailable with
// its reason: it is never captured from a fixture and never passes silently.
//
// The Python comparator (src/compare.py) owns the tolerance; this script only captures.
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { serve } from '../../browser-budgets/src/serve.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..');
const require = createRequire(import.meta.url);

function argument(name, fallback) {
  const index = process.argv.indexOf(`--${name}`);
  return index === -1 || index + 1 >= process.argv.length ? fallback : process.argv[index + 1];
}

function loadPlaywright() {
  const candidates = [
    process.env.SPOT_PLAYWRIGHT_HOME && path.join(process.env.SPOT_PLAYWRIGHT_HOME, 'node_modules', 'playwright'),
    path.join(root, '..', 'browser-budgets', 'node_modules', 'playwright'),
    path.join(root, 'node_modules', 'playwright'),
  ].filter(Boolean);
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return require(candidate);
  }
  return require('playwright');
}

const publish = path.resolve(argument('root', process.env.SPOT_BROWSER_PUBLISH || 'artifacts/browser-publish/wwwroot'));
const out = path.resolve(argument('out', path.join(root, 'results', 'captures')));
const only = argument('only', null);
const onlyViewport = argument('viewport', null);
const readyTimeout = Number(argument('ready-timeout-ms', '30000'));

if (!fs.existsSync(path.join(publish, 'index.html'))) {
  console.error(`capture.mjs: no published browser build at ${publish}`);
  console.error('capture.mjs: build it with: dotnet publish clients/avalonia/src/Spotledger.Client.Browser -c Release -p:SpotBrowserAot=false -o artifacts/browser-publish');
  process.exit(2);
}

const scenarios = JSON.parse(fs.readFileSync(path.join(root, 'scenarios.json'), 'utf8'));
let chromium;
try {
  ({ chromium } = loadPlaywright());
} catch {
  console.error('capture.mjs: playwright is not installed. Install it where WP-UI-09.T3 pins it:');
  console.error('  (cd tools/browser-budgets && npm ci && npx --no-install playwright install chromium)');
  console.error('  or set SPOT_PLAYWRIGHT_HOME to a directory whose node_modules holds playwright.');
  process.exit(3);
}

const manifest = {
  version: 1,
  commit: process.env.GITHUB_SHA || 'local',
  viewports: scenarios.viewports,
  captures: [],
  unavailable: [],
  metrics: {},
};
const viewportWanted = (viewport) => !onlyViewport || viewport.name === onlyViewport;
const scenarioWanted = (scenario) => !only || scenario.id === only;

// Step through one scenario. Returns false when the build cannot reach the state; the reason is
// recorded by the caller from the last returned message.
async function drive(page, scenario) {
  for (const step of scenario.steps) {
    if (step.openList) {
      const opened = await page.evaluate((doctype) => window.spotBrowser.openList(doctype), step.openList);
      if (!opened) return { reachable: false, reason: `openList(${step.openList}) returned false: the shell cannot open a list before sign-in` };
    } else if (step.openForm) {
      const [doctype, id] = step.openForm;
      const opened = await page.evaluate(([d, i]) => window.spotBrowser.openForm(d, i), [doctype, id]);
      if (!opened) return { reachable: false, reason: `openForm(${doctype}, ${id}) returned false: the shell cannot open a form before sign-in` };
    }
  }
  return { reachable: true };
}

const server = await serve(publish);
const browser = await chromium.launch({ args: ['--force-device-scale-factor=1', '--font-render-hinting=none'] });
let failures = 0;
try {
  fs.mkdirSync(out, { recursive: true });
  for (const viewport of scenarios.viewports.filter(viewportWanted)) {
    const context = await browser.newContext({
      viewport: { width: viewport.width, height: viewport.height },
      deviceScaleFactor: 1,
      colorScheme: 'light',
      reducedMotion: 'reduce',
      locale: 'en-GB',
      timezoneId: 'UTC',
    });
    for (const scenario of scenarios.scenarios.filter(scenarioWanted)) {
      const page = await context.newPage();
      try {
        await page.goto(server.url, { waitUntil: 'domcontentloaded' });
        await page.evaluate((timeout) => Promise.race([
          window.spotBrowser.ready,
          new Promise((_, reject) => setTimeout(() => reject(new Error(`ready did not resolve within ${timeout} ms`)), timeout)),
        ]), readyTimeout);
        const metrics = await page.evaluate(() => JSON.parse(window.spotBrowser.metrics()));
        manifest.metrics[`${scenario.id}-${viewport.name}`] = { ready: metrics.ready, marksUs: metrics.marksUs || null };
        if (scenario.unavailable) {
          manifest.unavailable.push({ id: scenario.id, viewport: viewport.name, reason: scenario.unavailable });
          continue;
        }
        const outcome = await drive(page, scenario);
        if (!outcome.reachable) {
          manifest.unavailable.push({ id: scenario.id, viewport: viewport.name, reason: outcome.reason });
          continue;
        }
        // Two frames after the last state change, with animations off: the same pixels every run.
        await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
        const file = `${scenario.id}-${viewport.width}x${viewport.height}.png`;
        await page.screenshot({ path: path.join(out, file), animations: 'disabled', caret: 'hide' });
        manifest.captures.push({ id: scenario.id, viewport: viewport.name, width: viewport.width, height: viewport.height, file });
        console.log(`captured ${file}`);
      } catch (error) {
        failures += 1;
        manifest.unavailable.push({ id: scenario.id, viewport: viewport.name, reason: `error: ${error.message}` });
        console.error(`capture.mjs: ${scenario.id} @ ${viewport.name} failed: ${error.message}`);
      } finally {
        await page.close();
      }
    }
    await context.close();
  }
} finally {
  await browser.close();
  await server.close();
}

fs.writeFileSync(path.join(out, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
console.log(`capture.mjs: ${manifest.captures.length} capture(s), ${manifest.unavailable.length} unavailable, ${failures} error(s); manifest ${path.join(out, 'manifest.json')}`);
if (failures > 0 || manifest.captures.length === 0) process.exit(1);
