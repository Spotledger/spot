# Spotledger.Client.Browser.Shell (WP-UI-09.T2)

The cached app shell of the browser build, as plain .NET code with no Avalonia and no browser API. The service
worker (`../Spotledger.Client.Browser/wwwroot/sw/`) carries the decisions out; this library is where they are
made and tested. Part 1 (WP-UI-09.T1) produced `assets.json`; see `clients/avalonia/docs/browser-publish.md`.
The AOT choice of the head is provisional until the SPK-07 re-run; nothing here depends on it, and a plain
`dotnet build` or `dotnet test` needs no workload.

## Pieces

| Type | Decides |
| --- | --- |
| `ShellManifest` | Parses `assets.json` and fails closed: version, build id recomputed from the files, lower-case SHA-256s, sorted unique plain paths. |
| `ShellCachePlanner` | Given the version record, the keys present and the new manifest: which entries to fetch, keep and delete. A cache key is `path#sha256`, so a changed file is a new key; the `.br` and `.gz` siblings are not cached; a contract-version change fetches everything again; it never lists a needed key for deletion. |
| `ShellInstaller` | Two steps over the ports below. `StageAsync` downloads, checks each file against its manifest hash and writes a `pending` record; `ActivateAsync` makes it the `active` record and only then removes unneeded keys, so a running page never loses a file. A cold start activates at once. A mismatch or a network error commits nothing. |
| `UpdateStateMachine` | idle, update-found, update-ready, applying. The prompt shows only while the app reports no unsaved edits; an apply requested during an edit is deferred and starts when the last edit is saved; until the app has reported once, edits are assumed (fail closed). |
| `OfflinePolicy` | The shell (files of the active build, and any page navigation as `index.html`) is cache first. Everything else, every non-GET and any shell file asked with a query string is network only: never read from, never written to the cache, refused when offline. |
| `LazyModuleRegistry`, `ModuleMap` | Module id to assemblies (`modules.json`, version 1). Loads on demand, once, concurrent callers share one load; only assemblies listed in the shell manifest; unknown ids and unsafe names are refused; a failure gives a plain message and can be retried. No third-party assemblies (SPK-08). |
| `ViewLoadTracker` | Skeleton until the module, the metadata and the view definition have arrived; failed with a retry. |
| `HashedDocumentCache` | Metadata and view-definition documents stored under their own SHA-256 and re-checked on read. Only those two kinds are accepted. |

Ports (`Ports.cs`): `IShellStorage` (blobs by key plus small named records; the Cache API in the browser),
`IShellNetwork`, `IModuleLoader`, `IContentHasher`. The library reads no clock and no random. The names differ from
the head's `IBrowserStorage` (a synchronous string store of the page) on purpose: that one cannot hold blobs.

## The service worker

`wwwroot/sw/spot-sw.js` is the Cache API and `postMessage` plumbing only; `shell-policy.js` is its pure policy, a
line-for-line mirror of the C# above; `shell-bridge.js` is the page side (registration and the prompt).

- Registration is in `index.html`: `<meta name="spot-contract">` (the native API contract version; the worker clears
  its cache when it changes) and two deferred scripts. The first-paint path is untouched.
- Page to worker: `{type:'spot-shell', cmd:'check'|'status'}`. Worker to page: `{type:'spot-shell', event:
  'update-found'|'update-ready'|'update-failed'|'activated'|'status', build}`.
- The worker never stores a data response, a session token or anything not listed in `assets.json`; a hash mismatch
  stores nothing. A staged build becomes active on the next page load (a navigation), "Update now" simply reloads.
- App hook: `window.spotShell.setUnsavedEdits(true|false)`. Tokens stay in memory only; this code uses no
  `localStorage` or cookies.

## Tests

`dotnet test clients/avalonia/tests/Spotledger.Client.Browser.Shell.Tests`: fake storage and network (cold, warm,
one changed file, contract change, update during an edit, retryable module load, never deleting a needed key, offline
shell versus data), golden vectors (`Golden/vectors.json`) run through the C# code and through `shell-policy.js`, and
the real `spot-sw.js` run under Node in a vm over a fake Cache API. If `node` is not installed the two Node tests
return without checking; run them by hand from the test project folder: `node Js/sw-policy.test.mjs` and
`node Js/sw-worker.test.mjs`.

## Not done here (outside the owned paths of this task)

- Wiring the library into the head: a project reference, `BrowserApp` using `LazyModuleRegistry` and `ViewLoadTracker`
  for the real form and list tabs, and calling `window.spotShell.setUnsavedEdits` from the shell. The head's
  project file and `Logic/` are not owned by this task.
- `modules.json` is not produced by the publish step yet (the format is above); without it no module loads.
- `spot-http` serving (versioned path, `Cache-Control`, `Service-Worker-Allowed` for the app root).
- The budgets (`tools/browser-budgets`) wait for the SPK-07 AOT re-run.
