// Starts the .NET runtime and hands the hooks to spot-browser.mjs. Kept small: the first-paint path
// is this file, the runtime and the shell. Nothing else is fetched before the shell is on screen.
import { dotnet } from './_framework/dotnet.js';
import { attach, mark } from './spot-browser.mjs';

mark('scriptStart');
const runtime = await dotnet.withDiagnosticTracing(false).create();
mark('runtimeCreated');
const config = runtime.getConfig();
const exports = await runtime.getAssemblyExports(config.mainAssemblyName);
attach(exports.Spotledger.Client.Browser.BrowserHooks);
await runtime.runMain(config.mainAssemblyName, []);
