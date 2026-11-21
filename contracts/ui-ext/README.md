# UI extension contract v0.1 (tier A)

Status: **draft for the architecture owner's approval** (like `contracts/events`). Written by the lead, WP-UI-13.T1.
Normative companion: ADR-069 amendment A1. Implementations: WP-UI-13.T2 (Rust validator and
installer) and WP-UI-13.T3 (Avalonia renderer); both run the corpus in `examples/`.

Tier A (ADR-27, ADR-28) means an app extends the client by **declaring data**. It never ships client
code. Behaviour happens only through server-side methods the app's manifest declares, and through the
shared expression language, which the client and the server evaluate with identical results.

## What a tier A extension can and cannot do

| Can | Cannot |
| --- | --- |
| Add **actions** (button, menu item) to the closed slots, each with exactly one effect | Add chrome, a window, a control, or a slot |
| `call` a method its manifest declares, with arguments written as expressions over `doc` | Run script in the client, or any event handler beyond a declared effect |
| `open` a registered destination, with filters as expressions | Navigate to an external URL or load remote content (images, fonts, frames) |
| `navigate_url` to a path under a route its own manifest declares | Inject markup: a `text` block takes a restricted markdown subset, no HTML |
| Add **panels** (`doc.sidepanel`, `list.sidepanel`), **tabs** (`doc.tab`) and **tiles** (`home.tiles`) | Fetch data any way but a **server query** the app declared, so permissions apply |
| Define **composite components** from catalog controls and layout primitives | Define a second layout language, a new control, or recursion without a bound |
| Hide or show an item with `visible_if` / `enabled_if` | Widen what a user may see or do: display is never authorisation |

The rules, one each:

1. **No script.** Every behaviour is a declared `call` effect or an expression. An expression is pure and
   total (`docs/expression-language.md`): no clock but `now()`, no I/O, no loops. Anything that needs
   more is tier B (WP-UI-14).
2. **No remote content.** There is no URL in the contract except `navigate_url`, and that is a path under
   the app's own routes. Icons are tokens of a closed set, never URLs or files.
3. **No markup injection.** A `text` block is a markdown subset: paragraphs, emphasis, strong, lists,
   inline code and links `[text](dest:app.name)` to registered destinations. HTML, images and other links
   are refused (`uiext.raw_html`). The client renders; it never evaluates.
4. **No unbounded recursion.** Components form an acyclic graph; one expansion has depth at most 8 and at
   most 500 nodes (a `repeat` counts `max` times its body); `repeat.max` is at most 50.
5. **No widening of permissions.** The server decides everything again: a `call` runs under the user's
   rights; a `table`, `stat` or `field_list` shows only what the user's query returns; `permission` on an
   item only *hides* it for a user who lacks that right. The bundle is presentation (ADR-069 section 7).

## Slots (the closed list)

`schema/slot.json` is normative; `x-slots` holds this table for tools.

| Slot | Accepts | Max items per app (per DocType in `doc.*`/`list.*`) | Label max |
| --- | --- | --- | --- |
| `shell.sidebar` | action | 10 | 32 |
| `shell.menubar` | action | 10 | 32 |
| `shell.statusbar` | action | 3 | 24 |
| `shell.user-menu` | action | 5 | 32 |
| `home.tiles` | tile | 8 | 40 |
| `doc.toolbar` | action | 6 | 24 |
| `doc.sidepanel` | panel | 3 | 32 |
| `doc.tab` | tab | 3 | 24 |
| `list.toolbar` | action | 4 | 24 |
| `list.sidepanel` | panel | 3 | 32 |

**Ordering** (same in every slot, ADR-069 section 3.2): the dependency order of the contributing apps,
then `order` ascending, then app name, then id. An item in a `doc.*` or `list.*` slot names its `doctype`
and appears only on that DocType.

## The concepts

- **Action** (`schema/action.json`): `id` (`app.name`), `label` (a `key:` translation key or a literal),
  `icon` (token), `slot`, `order`, `visible_if`, `enabled_if`, `permission` (`<doctype>:<right>` that
  must exist), `confirm` (text), and exactly one effect.
  - `call { doctype, method, args, then }`: `then` is `refresh`, `show_message`, `download`, or
    `{open: destination}`. **Idempotency and busy state:** the client sends one `Idempotency-Key` per
    click; while the call is in flight the action is disabled and a second click does nothing; a retry
    after a lost answer re-sends the same key and gets the stored answer; a mutating method sends the
    document's `If-Match` version (ADR-055). `confirm`, if present, is shown before the first send.
  - `open { destination, filters }`: a registered destination; each filter value is an expression.
  - `navigate_url`: a path under a declared route (`uiext.external_url`, `uiext.unknown_route`).
- **Panel** (`schema/panel.json`): an ordered list of at most 12 blocks: `field_list`, `key_value`, `text`,
  `table` (query, at most 8 columns, `max_rows` at most 50, rows link to the document), `stat` (an
  aggregate query with a format), `list_of_links`, `empty_state`. Data blocks name a server query the
  app declares (`uiext.query_missing`, `uiext.unknown_query`, `uiext.query_kind`).
- **Composite component** (`schema/component.json`): `props` (typed from the field vocabulary), named
  `slots` (holes), and a `body` of `row`, `column`, `grid`, `group`, `repeat`, `when`, `use`, `slot` and
  `control` nodes. Controls are the **read-only presentations of the ADR-068 catalog** (`label`,
  `heading`, `separator`, `badge`, `progress`, `number`, `currency`, `date`) plus `button`, whose only
  event is `on_click` naming an action of the same app. Inside a component, expressions read the props as
  `doc.<prop>` (the language has no other root; a component is evaluated with its props as the `doc`).
- **Tile** (`home.tiles`): a composite plus its props, each a literal or `{query}` (an aggregate query).
- **Tab** (`doc.tab`): a label and a panel of the same app.
- **`ui` document** (`schema/bundle.json`): `contract_version`, `requires_shell` (the minimum shell; an
  older shell keeps its cached bundle and shows an upgrade notice, ADR-069 section 5.6), the `app`, and
  the arrays above. Every item carries `origin = app:<name>`; its id is namespaced by that app.

## Install-time validation

The installer (WP-UI-13.T2) refuses a bundle with any `uiext.*` finding (`errors.md`), naming app, file,
id and JSON pointer, and loads nothing. The renderer (WP-UI-13.T3) treats a bundle it cannot render
(unknown slot or node kind, a newer `contract_version`) as absent for that item and shows the notice,
never a partial or guessed rendering.

## Files

`schema/` JSON Schema 2020-12 (one file per concept, `$id`s carry the version) · `examples/valid/` ·
`examples/invalid/` (one file per rule, header `expect: <code>`) · `examples/sample-app/` (the sample
app's `ui.json` and the `context.json` that stands in for what its manifest declares) · `errors.md` ·
`check.py` (reference validator and corpus runner; run `python check.py` with `requirements.txt` installed; wiring it into CI is a follow-up outside this
task's owned paths).

## Open questions (for the architecture owner)

1. **Expression roots.** The language has only `doc` and `parent`. The brief's `selection` and `user` are
   not roots. v0.1 resolves it thus: `list.*` and `shell.*` actions take literal arguments only, and the
   host always sends the selected ids as `ids`; visibility by user is `permission`, not an expression.
   Adding roots is a language change in both implementations (WP-UI-11), so it is not made here. Confirm.
2. **Icon set.** The tokens in `schema/common.json` are 52 neutral names. The design system
   (`clients/avalonia/design/tokens.json`) has no icon section yet; whoever adds it must cover this list.
3. **Queries.** Apps declare server queries (`rows` and `aggregate` kinds). The manifest has no such
   section today (`spot-manifest`); `context.json` stands in. A manifest `[ui]` section (ADR-069 section 3,
   WP-EXT-04 follow-up) must declare `queries`, `destinations` and `routes`. Who owns the query result
   shape (columns, formats) is left to WP-UI-13.T2.
4. **Translation keys.** `key:<id>` is the marker; the translation catalogue is not defined yet.
5. **Tabs and panels** reuse one panel definition (a tab shows a panel); there is no inline tab body.
6. **Tier B.** Anything needing script, a custom control or remote content: WP-UI-14.
