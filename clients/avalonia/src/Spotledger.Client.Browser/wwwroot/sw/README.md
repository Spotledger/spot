# sw/: the shell service worker (WP-UI-09.T2)

| File | What |
| --- | --- |
| `spot-sw.js` | The service worker. Carries out the cache decisions with the Cache API; answers only shell files and page navigations; every other request (data above all) goes to the network untouched. |
| `shell-policy.js` | The policy as pure functions (cache plan, request classification, update state machine). A mirror of the C# library `Spotledger.Client.Browser.Shell`; golden vectors run the same cases through both. |
| `shell-bridge.js` | The page side: registers the worker, shows the dismissible "update ready" prompt, exposes `window.spotShell`. |

Read `../../../Spotledger.Client.Browser.Shell/README.md` for the rules and the message protocol. In short:

- The worker reads `assets.json` (revalidated every time), stores a file only after its SHA-256 matched the
  manifest, and keys the cache by `path#sha256`, so a changed file is a new entry.
- A new build is staged beside the running one and activates on the next page load; the prompt is offered only
  while the app reports no unsaved edits (`window.spotShell.setUnsavedEdits`).
- A changed `spot-contract` meta value (the native API contract version) clears the cache.
- The worker needs the response header `Service-Worker-Allowed` for the app root when it is served from `sw/`
  (an open question for `spot-http`, see the library README).

`node tests/Spotledger.Client.Browser.Shell.Tests/Js/sw-policy.test.mjs` runs the Node test of `shell-policy.js`
by hand (the unit test project runs it too when `node` is installed).
