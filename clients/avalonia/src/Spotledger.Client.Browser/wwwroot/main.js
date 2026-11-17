// Starts the .NET runtime and hands the hooks to spot-browser.mjs. Kept small: the first-paint path
// is this file, the runtime and the shell. Nothing else is fetched before the shell is on screen.
import { dotnet } from './_framework/dotnet.js';
import { attach, mark } from './spot-browser.mjs';

mark('scriptStart');
const runtime = await dotnet.withDiagnosticTracing(false).create();
mark('runtimeCreated');
const config = runtime.getConfig();
const exports = await runtime.getAssemblyExports(config.mainAssemblyName);
// The test sign-in hook is on only when the page was loaded with ?testSignIn=1 (read once, here). Only the flag
// is read from the URL: credentials are never taken from it.
const testSignIn = new URLSearchParams(globalThis.location?.search ?? '').get('testSignIn') === '1';
const hooks = exports.Spotledger.Client.Browser.BrowserHooks;
if (testSignIn) hooks.EnableTestSignIn();
attach(hooks, { testSignIn });
await runtime.runMain(config.mainAssemblyName, []);
