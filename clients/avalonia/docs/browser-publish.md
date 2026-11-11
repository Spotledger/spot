# Publishing the browser build

The browser head is `src/Spotledger.Client.Browser` (Avalonia.Browser, `net10.0-browser`). It runs the
shared client libraries as WebAssembly in the page. This document is the contract between the build,
the server that serves it and the budget scripts (`tools/browser-budgets/`, WP-UI-09.T3). The decision
behind the target is `docs/spikes/SPK-07/REPORT.md` (conditional go: AOT build). **The AOT choice is
provisional until the SPK-07.T2 re-run:** `docs/spikes/SPK-07/VERIFICATION.md` records that the AOT publish,
every AOT row and both static-link rows were not independently reproduced, and asks for a re-run of
`lab/SPK-07/run.sh` on a quiet host with at least 20 GB free. No budget is fixed by this document.

## Build and publish

One-time setup: the .NET SDK pinned in `global.json` and its WebAssembly workload.

```
dotnet workload install wasm-tools
dotnet publish clients/avalonia/src/Spotledger.Client.Browser -c Release -o artifacts/browser-publish
```

The Release profile (in the project file) is: AOT (`RunAOTCompilation`, publish only), full trimming (`TrimMode=full`),
WebAssembly SIMD, invariant globalization (no ICU data), and Brotli plus gzip pre-compression next to
every file (`foo.wasm.br`, `foo.wasm.gz`). A Debug build is interpreted and not trimmed; use it only to
develop. AOT takes several minutes and several GB of disk; keep `artifacts/` out of commits.

A plain `dotnet build` or `dotnet test` of the solution does not need the workload: AOT and the native
libraries are used only by `dotnet publish`, and a build drops the native references that only a publish links.
Only a publish needs `dotnet workload install wasm-tools`; a CI job that publishes must install it first.

AOT can be switched off from the command line with the `SpotBrowserAot` property (default `true`):

```
dotnet publish clients/avalonia/src/Spotledger.Client.Browser -c Release -p:SpotBrowserAot=false -o artifacts/browser-publish
```

This gives the interpreted (trimmed) build that the SPK-07 spike measured, and is the fallback if the re-run
does not confirm the AOT numbers.

The web root of the output is `artifacts/browser-publish/wwwroot/`. Serve that folder.

### Trimming: what is kept and why

`linker-roots.xml` names the assemblies the trimmer must keep and explains each one. The reason is the
URI-based resource loading of the theme and control libraries (SPK-07 risk 1): without the roots the
published app stops at start-up with `No precompiled XAML found for avares://...`. The roots cost size.
They go away when those libraries load their resources without a URI (open question 2).

Trimming and AOT warnings from libraries outside this project (`IL2026`, `IL2104`, `IL3050`, `IL3053`) are
not errors in this project only; every other warning is still an error.

## The integrity manifest: `assets.json`

After publish a target writes `wwwroot/assets.json`:

```json
{
  "version": 1,
  "build": "<64 hex: SHA-256 over every path and hash, in path order>",
  "files": [
    { "path": "_framework/dotnet.js", "bytes": 12345, "sha256": "<64 hex>" }
  ]
}
```

- Every file under the web root is listed, sorted by path, including the `.br` and `.gz` siblings. The
  manifest does not list itself.
- Nothing time- or machine-dependent is written, so the same files give the same bytes.
- `build` changes if any file is added, removed or changed. Use it as the version of the whole build.
- The shell cache of WP-UI-09.T2 reads this file to decide what to cache and what changed.

## Serving it

The server serves the static shell at a versioned path (`/app/<build>/...`, with `<build>` from
`assets.json`) and nothing else about the build is dynamic.

| Files | Header |
| --- | --- |
| `index.html`, `assets.json`, `spot-browser.mjs`, `calc.mjs`, `main.*.js` placed under the version path | `Cache-Control: no-cache` (revalidate) for `index.html` and `assets.json` only |
| everything under `_framework/` and any file whose name carries a fingerprint | `Cache-Control: public, max-age=31536000, immutable` |
| `*.wasm`, `*.js`, `*.mjs`, `*.json`, `*.dll` etc. when a `.br` or `.gz` sibling exists | serve the sibling with `Content-Encoding: br` (preferred) or `gzip`, original `Content-Type` |
| `*.wasm` | `Content-Type: application/wasm` (streaming compile needs it) |

`spot-browser.mjs` and `calc.mjs` are not fingerprinted; serve them with revalidation, or place the whole build
under its versioned path and cache everything under it as immutable. The second is the recommended
shape: one immutable folder per build, one small redirect or `index.html` that points at the current one.

## Page hooks for the budget scripts

`window.spotBrowser` exists as soon as the page's module script runs, before the runtime starts. The
Playwright scripts need no other knowledge of the build.

| Hook | Type | Meaning |
| --- | --- | --- |
| `ready` | `Promise<void>` | Resolves when the shell (server picker, login or home) is interactive. |
| `openForm(doctype, id)` | `Promise<boolean>` | Waits for `ready`, then opens the form tab of the document. `false` when the shell cannot (not signed in). |
| `openList(doctype)` | `Promise<boolean>` | Waits for `ready`, then opens the list tab of the doctype. `false` when it cannot. |
| `metrics()` | `string` (JSON) | The measurements below. Parse with `JSON.parse`. |

In this task the form and list tabs are real tabs with the real key and title, but their content is a
placeholder line until the lazy module registry (WP-UI-09.T2) supplies the form and list views. The
hooks and the metrics are stable; the budgets that need real screens wait for T2 (open question 3).

### `metrics()`

```json
{
  "schema": 1,
  "ready": true,
  "marksUs":  { "dotnetStart": 0, "firstPaint": 0, "ready": 0 },
  "counters": { "openForm": 0, "openList": 0 },
  "labels":   { "calc": "managed", "storage": "available" },
  "js":       { "marksMs": { "moduleLoaded": 0, "scriptStart": 0, "runtimeCreated": 0, "ready": 0 } }
}
```

- `marksUs` are whole microseconds on the page's monotonic clock since its time origin; the first mark of
  a name wins. `dotnetStart` is when .NET code starts, `firstPaint` is the first frame of the shell,
  `ready` is when `ready` resolved, `lastFormOpened` and `lastListOpened` are the latest successful opens,
  `calcSwitched` is when the WebAssembly calculation module replaced the managed one.
- `js.marksMs` are the JavaScript marks (`performance.now()` in milliseconds; JavaScript has no integers).
- Keys are added, never renamed, within `schema` 1.

## The calculation path

Totals, tax and pricing previews run on the client with the shared expression language and its decimal
rules, never with floats. Two backends give the same outcome for the same request:

1. **managed** (always present): `Spotledger.Client.Expressions`, the same code the desktop client runs.
2. **wasm** (optional): a calculation module (`calc/spot_calc.wasm`) loaded by `calc.mjs` after the shell is
   ready. Before instantiating, `calc.mjs` hashes the downloaded bytes and compares the SHA-256 with the
   entry for `calc/spot_calc.wasm` in `assets.json`; a missing manifest, a missing entry or a different hash
   means the module is not used (fail closed) and the managed backend stays. It replaces the managed backend when it loads; if it fails during a request, the path counts
   the failure and answers that request and all later ones on the managed backend.

Requests and answers travel as JSON in the typed-value format of the conformance corpus
(`crates/framework/spot-expr/corpus/README.md`); integers and decimals are strings and a decimal keeps its
scale. The tests run the whole corpus (961 cases, including the decimal rounding cases of
`arithmetic.json` and `functions.json`) through both backends and the wire format, plus calculation cases
whose expected values were computed independently.

Proposed module ABI v1 (implemented by `wwwroot/calc.mjs`, no module exists yet): exports `memory`,
`spot_calc_abi() -> 1`, `spot_calc_alloc(len)`, `spot_calc_eval(ptr, len) -> answerLen`, `spot_calc_answer()`,
`spot_calc_free(ptr, len)`; request and answer are UTF-8 JSON in linear memory.

Dynamic loading of third-party .NET assemblies in the browser is **not** supported or promised
(SPK-08 found it unproven).

## Seams left for WP-UI-09.T2

- `wwwroot/sw/` is empty except for a README. T2 puts the service worker there.
- `wwwroot/index.html` has a commented registration point for the worker.
- The first-paint path starts only the shell (server picker, login, home). No module bundle is loaded;
  T2 adds progressive loading behind it.

## Open questions

1. **Server change in `spot-http`.** Static asset handling needs, for the versioned path above: serving a
   folder at `/app/<build>/`, preferring a `.br` (then `.gz`) sibling with `Content-Encoding`, the headers in
   the table, `application/wasm`, and a fallback to `index.html`. This task does not edit the server.
2. **Trim-safety of other libraries.** `Spotledger.Client.Api` uses reflection-based `System.Text.Json`
   (`ApiConnection.cs`, `RememberedUserStore.cs`) and `Spotledger.Client.Theme` / this head load resources
   with `ResourceInclude` by URI. Both need a fix in their own tasks; until then the roots and the
   warning exceptions above stay. The shell also reads JSON by reflection (`RecentServersStore`, `KeymapProfile`);
   a trimmed app disables that, so the head sets `JsonSerializerIsReflectionEnabledByDefault` (found by running
   the published build in headless Chromium: `window.spotBrowser.ready` resolved in about 1.7 s).
3. **Real form and list views** in the browser need the lazy module registry (T2).
4. **The calculation module** (the shared Rust crate compiled to `wasm32-unknown-unknown` with the ABI
   above) is not in the repository's owned paths for this task; it needs a crate and a build step.
5. **Session tokens** are kept in memory only in the browser (no OS secret store), so a reload signs the
   user out. Whether to keep a token in browser storage is a security decision.
6. **`SpotGrid` has no wheel or touch scrolling** (SPK-07 risk 2); it is a grid-lane task.
7. **No budget is fixed by this PR.** `budgets.toml` (WP-UI-09.T3) waits for the SPK-07 AOT re-run.
8. **Owners of the rest.** `tools/browser-budgets/` (and its acceptance line
   `python -m unittest discover -s tools/browser-budgets/tests -v`) belongs to WP-UI-09.T3, not this task.
   No task is planned to remove the blanket `WarningsNotAsErrors` list (`IL2026`..`IL3053`) from the project
   file; it can go only when the libraries in open question 2 are trim-safe.
9. **CI needs the workload to publish.** The CI job that runs `dotnet publish` of the head (WP-UI-09.T4) must
   run `dotnet workload install wasm-tools` first; the solution build and tests need no workload.
