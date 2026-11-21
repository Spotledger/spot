# Spotledger.Client.Extensions

Tier A UI extensions (ADR-069 amendment A1, ADR-27): the client half. The server
(`crates/framework/spot-ui-ext`, WP-UI-13.T2) validates and resolves a site's `ui`
documents into one canonical JSON document; this project turns that document into what the
client shows — and nothing else.

## Two projects, one owned path

| Project | TFMs | References | What it holds |
| --- | --- | --- | --- |
| `Spotledger.Client.Extensions` | `net10.0`, AOT-compatible | `Spotledger.Client.Metadata`, `Spotledger.Client.Expressions` | the model: parse, diagnostics, slot views, actions, panels, composites, markdown |
| `Spotledger.Client.Extensions/Renderer` | `net10.0` + Avalonia | `Spotledger.Client.Controls`, `Spotledger.Client.Theme` | the thin Avalonia hosts: `ActionBar`, `SidePanelHost`, `TileHost`, `ComponentPresenter`, `MarkdownPresenter` |

The model has no Avalonia reference (a test reads the assembly's references and fails if one
appears), so a second renderer — the browser head when it lands — reuses it unchanged. The
renderer decides nothing: every "what to show, is it enabled, what does a click send" answer
is in the model and is exercised headless.

## What the model guarantees

- **Parse never throws.** `ExtensionSet.Parse(json, facts, source)` reads a resolved bundle
  (`slots`) or one app's document (arrays). An item the client cannot render is dropped with
  an `ExtensionDiagnostic`; the rest of the document still renders. A newer contract major,
  a shell older than `requires_shell`, or a document over 1 MiB disables the set whole with
  one diagnostic (the shell shows the "update the app" notice).
- **Codes are the contract's.** Structural and reference findings use `contracts/ui-ext/errors.md`
  codes one for one; the three refusals only a client can make are prefixed
  `uiext.client.*` (`unsupported_contract`, `document_too_large`, `expression_failed`,
  `dispatch_failed`, `query_failed`).
- **Expressions are values, not exceptions.** A broken or failing expression evaluates to
  hidden/disabled and reports once (`uiext.expression_invalid` or
  `uiext.client.expression_failed` with the `expr.*` code in the message).
- **The contract's limits hold again on the client**: slots, per-app limits, labels, icons,
  effect count, closed objects, panel blocks, component depth 8 / 500 nodes / repeat 50.
- **One click, one request.** `ActionModel.InvokeAsync` dispatches once with one idempotency
  key, is busy (and unclickable) while in flight, sends the document version for the open
  document, asks the shell to confirm when `confirm` is set, and applies the answer through
  `IShellServices`. A failure becomes one message with the server's problem text.
- **Data blocks come from declared queries only**, are asynchronous and cancellable, page
  inside `max_rows`, and show the empty and error-with-retry states. Saving the document
  refreshes them (`PanelModel.DocumentSavedAsync`).
- **No floats, no clock, no reflection.** Decimals stay exact (mantissa/scale from the
  canonical text), `now` is an argument, and JSON is read with `JsonDocument`.

## Consuming it

```csharp
ExtensionSet set = ExtensionSet.Parse(json, ExtensionFacts.FromContextJson(manifestJson));
ExtensionContext context = ExtensionContext.FromDocument(draft, now, locale, user, permissions, selection);
ExtensionServices services = new(dispatcher, shellServices, queryRunner);

IReadOnlyList<string> slots = ExtensionSlots.Register(registry, set, context, services);
```

`ExtensionSlots.Register` asks the shell (`IShellSlotRegistry`) for one host per populated
slot; the shell passes the DocType when it builds a `doc.*`/`list.*` slot. The seam exists so
this project never edits `Spotledger.Client.Shell`; what the shell still needs is listed in
the task's open questions.

## Reading the corpus

The tests read `contracts/ui-ext/` from the repository — the same bytes the Rust validator
uses — and `crates/framework/spot-ui-ext/tests/golden/sample-app.resolved.json`, the resolved
sample app. `Data/sample-app.resolved.json` is a copy that a test compares with the golden, so
a server-side change fails here instead of drifting. Every invalid corpus file must yield its
`expect: <code>` diagnostic without an exception.

## Known gaps (owned elsewhere)

- The catalog has no read-only presentations for `label`, `heading`, `separator`, `badge`,
  `number`, `currency`, `date`, so the renderer draws them with theme-token primitives; it
  uses `SpotButton` where the catalog has a button.
- Tabs (`doc.tab`) are parsed and carried by the model; the shell has no tab strip seam yet.
- No browser head project exists in the solution, so `net10.0-browser` is not built here; the
  model's reference test and `targets-check.md` record how the gap is checked.
