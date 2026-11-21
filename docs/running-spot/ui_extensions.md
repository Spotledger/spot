Part of [Running Spot](../running-spot.md).

# Extending the client with data (tier A)

An app can add buttons, side panels, tabs, home tiles and small composite widgets to the client
**without shipping any client code**: it writes a `ui` document, in TOML or JSON, and the platform checks
it when the app is installed. This page is a guide for the app author. The full rules are in the
contract, `contracts/ui-ext/README.md` (contract version 0.1.0), and the stable refusals in
`contracts/ui-ext/errors.md`. Every example on this page that is marked as tested is run by
`cargo test -p spot-ui-ext --test guide_examples` against the same facts as the contract's own corpus, so
the page cannot rot unnoticed. Anything the validator refuses is refused here too: where this page and
the validator disagree, the validator is right.

## What you can and cannot do

- You can add **actions** to a fixed list of slots, each doing exactly one thing: call a method your app
  declares, open a registered destination, or go to a path under a route your app declares.
- You can add **panels**, **tabs** and **tiles** that show data from a **server query** your app declares,
  and **composite components** built from the shared controls and layout primitives.
- You can hide or disable an item with an expression in the shared language
  (the expression language, `docs/expression-language.md`).
- You cannot run script in the client, load anything from a URL, put HTML in a text block, add a slot, or
  recurse without a bound. Anything that needs more is tier B, a separate contract.
- Display is never authorisation: the server decides again every time (see Visibility and enablement).

## Your first extension

The smallest useful document adds one button to the toolbar of the `task` form that calls a method of
your app. The same document in TOML and in JSON (the file name decides the format: `.toml` is TOML,
anything else is JSON):

<!-- ui-example: valid file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.start_work"
origin = "app:sample"
label = "Start work"
icon = "play"
slot = "doc.toolbar"
doctype = "task"
order = 10
visible_if = "doc.status == 'Draft'"
permission = "task:write"

[actions.call]
doctype = "task"
method = "start_work"
then = "refresh"

[actions.call.args]
task = "doc.name"
```

<!-- ui-example: valid file=ui.json -->
```json
{
  "contract_version": "0.1.0",
  "requires_shell": "0.1.0",
  "app": "sample",
  "actions": [
    {
      "id": "sample.start_work",
      "origin": "app:sample",
      "label": "Start work",
      "icon": "play",
      "slot": "doc.toolbar",
      "doctype": "task",
      "order": 10,
      "visible_if": "doc.status == 'Draft'",
      "permission": "task:write",
      "call": {
        "doctype": "task",
        "method": "start_work",
        "args": { "task": "doc.name" },
        "then": "refresh"
      }
    }
  ]
}
```

What each line is for: `app` names your app, and every id is `<app>.<name>` with `origin` `app:<app>`; `slot`
and `doctype` place the button on the `task` form only; `order` sorts it among the others; `visible_if` shows
it only while the document is a draft; `permission` hides it from a user who lacks `task:write`; the
`call` names a method **your manifest declares** for that DocType, with its arguments written as
expressions over `doc`. The user sees a button "Start work"; a click sends one request under their own
rights, with one idempotency key, and the button stays disabled while it is in flight.

The checks run at install time against what your app declares. The examples on this page are checked
against the facts of the contract's sample app, which are:

<!-- ui-example: fragment -->
```json
{
  "methods": { "task": ["start_work", "close_task"], "project": ["archive"] },
  "queries": {
    "sample.open_tasks": { "kind": "aggregate", "doctype": "task" },
    "sample.task_count": { "kind": "aggregate", "doctype": "task" },
    "sample.recent_time_entries": { "kind": "rows", "doctype": "time_entry" }
  },
  "destinations": ["sample.projects", "sample.tasks", "sample.time_entries", "sample.reports"],
  "routes": ["/sample/reports"],
  "permissions": ["project:read", "project:write", "task:read", "task:write", "task:create", "time_entry:read", "time_entry:create"]
}
```

Not run: this is an excerpt of `contracts/ui-ext/examples/sample-app/context.json`, which stands in for the app manifest (the manifest has no section for queries, destinations and routes yet), not a `ui` document.

## Slots

The slot list is closed and `contracts/ui-ext/schema/slot.json` is its source. An item names its slot, and in a
`doc.*` or `list.*` slot also its `doctype`; it then appears only on that DocType. The limit is per app (per
DocType in `doc.*` and `list.*` slots), and a literal label may not be longer than the slot's label maximum.

| Slot | Accepts | Items | Label max |
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

Within a slot the order is: the apps in dependency order, then `order` ascending, then app name, then id.

Mistakes that are easy to make. A slot that does not exist:

<!-- ui-example: invalid code=uiext.unknown_slot file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.start_work"
origin = "app:sample"
label = "Start work"
slot = "doc.footer"
doctype = "task"
navigate_url = "/sample/reports"
```

An action placed in a slot that takes panels:

<!-- ui-example: invalid code=uiext.slot_kind_mismatch file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.start_work"
origin = "app:sample"
label = "Start work"
slot = "doc.sidepanel"
doctype = "task"
navigate_url = "/sample/reports"
```

A `doc.*` slot with no `doctype`:

<!-- ui-example: invalid code=uiext.doctype_required file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.start_work"
origin = "app:sample"
label = "Start work"
slot = "doc.toolbar"
navigate_url = "/sample/reports"
```

A label longer than the slot allows (`doc.toolbar` takes 24 characters):

<!-- ui-example: invalid code=uiext.label_too_long file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.start_work"
origin = "app:sample"
label = "Start working on this task right now"
slot = "doc.toolbar"
doctype = "task"
navigate_url = "/sample/reports"
```

Too many items in a slot is `uiext.slot_limit`: more than 6 actions of your app on one DocType's `doc.toolbar`;
group them in a panel instead. Ids and origins must belong to your app: an `id` that does not start with
`<your app>.` is refused,

<!-- ui-example: invalid code=uiext.origin_mismatch file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "other.start_work"
origin = "app:sample"
label = "Start work"
slot = "shell.sidebar"
navigate_url = "/sample/reports"
```

and so is an id used twice in one document (`uiext.duplicate_id`).

An icon is a token of a closed set (`add`, `edit`, `delete`, `save`, `search`, `play`, `time`, `chart` and
the others of `contracts/ui-ext/schema/common.json`), never a URL or a file:

<!-- ui-example: invalid code=uiext.unknown_icon file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.reports"
origin = "app:sample"
label = "Reports"
icon = "rocket"
slot = "shell.sidebar"
navigate_url = "/sample/reports"
```

## Actions and their effects

An action has **exactly one** of `call`, `open` or `navigate_url`; none or more than one is
`uiext.effect_invalid`.

`call` runs a server method your manifest declares for the DocType. `args` are expressions over `doc`;
`then` is `refresh`, `show_message`, `download` or `{ open = { destination = "..." } }`; `confirm` is shown
before the first send. A method that is not declared:

<!-- ui-example: invalid code=uiext.unknown_method file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.start_work"
origin = "app:sample"
label = "Start work"
slot = "doc.toolbar"
doctype = "task"

[actions.call]
doctype = "task"
method = "start_everything"
```

After a call you can open a destination, and a `confirm` text is shown before anything is sent:

<!-- ui-example: valid file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.close_task"
origin = "app:sample"
label = "Close task"
icon = "check"
slot = "doc.toolbar"
doctype = "task"
order = 30
confirm = "Close this task?"
permission = "task:write"

[actions.call]
doctype = "task"
method = "close_task"
then = { open = { destination = "sample.tasks" } }

[actions.call.args]
task = "doc.name"
```

Two effects on one action (here `open` and `navigate_url`) are `uiext.effect_invalid`, and so is none:

<!-- ui-example: invalid code=uiext.effect_invalid file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.both"
origin = "app:sample"
label = "Both"
slot = "shell.sidebar"
navigate_url = "/sample/reports"

[actions.open]
destination = "sample.tasks"
```

`open` goes to a **registered destination** (the destination list is the registry of the installed apps;
`sample.time_entries` is one of the sample app's). Each filter is an expression, so the target list can be
narrowed by the current document:

<!-- ui-example: valid file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.open_time"
origin = "app:sample"
label = "Time entries"
icon = "time"
slot = "doc.toolbar"
doctype = "task"
order = 20
permission = "time_entry:read"

[actions.open]
destination = "sample.time_entries"

[actions.open.filters]
task = "doc.name"
```

An unregistered destination is `uiext.unknown_destination`:

<!-- ui-example: invalid code=uiext.unknown_destination file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.open_time"
origin = "app:sample"
label = "Time entries"
slot = "doc.toolbar"
doctype = "task"

[actions.open]
destination = "sample.nowhere"
```

`navigate_url` is an absolute path under a route your manifest declares, never an address:

<!-- ui-example: valid file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.reports"
origin = "app:sample"
label = "Reports"
icon = "chart"
slot = "shell.sidebar"
navigate_url = "/sample/reports/summary"
```

A scheme or host is `uiext.external_url`:

<!-- ui-example: invalid code=uiext.external_url file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.out"
origin = "app:sample"
label = "Out"
slot = "shell.sidebar"
navigate_url = "https://elsewhere.example.invalid/page"
```

and a path that no declared route covers is `uiext.unknown_route`:

<!-- ui-example: invalid code=uiext.unknown_route file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.out"
origin = "app:sample"
label = "Out"
slot = "shell.sidebar"
navigate_url = "/other/page"
```

`permission` is `<doctype>:<right>` and must name a permission that exists:

<!-- ui-example: invalid code=uiext.unknown_permission file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.start_work"
origin = "app:sample"
label = "Start work"
slot = "doc.toolbar"
doctype = "task"
permission = "task:launch_missiles"
navigate_url = "/sample/reports"
```

Objects are closed: a property the contract does not define (an event handler, a script) is refused as
`uiext.schema`, whatever it is called:

<!-- ui-example: invalid code=uiext.schema file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.start_work"
origin = "app:sample"
label = "Start work"
slot = "shell.sidebar"
navigate_url = "/sample/reports"
on_click_script = "alert(1)"
```

## Panels, tabs and tiles

A **panel** (`doc.sidepanel`, `list.sidepanel`) is an ordered list of at most 12 blocks: `field_list`,
`key_value`, `text`, `table`, `stat`, `list_of_links` and `empty_state`. Data blocks (`table`, `stat`) name a
**server query** your app declares, so the user's permissions apply to what they show: a `table` needs a
query of kind `rows` (at most 8 columns and 50 rows), a `stat` one of kind `aggregate`. A **tab**
(`doc.tab`) shows a panel of the same app on a DocType's form.

<!-- ui-example: valid file=ui.json -->
```json
{
  "contract_version": "0.1.0",
  "requires_shell": "0.1.0",
  "app": "sample",
  "panels": [
    {
      "id": "sample.task_overview",
      "origin": "app:sample",
      "label": "Overview",
      "slot": "doc.sidepanel",
      "doctype": "project",
      "order": 0,
      "permission": "project:read",
      "blocks": [
        { "type": "field_list", "fields": ["status", "owner", "budget"] },
        { "type": "stat", "query": "sample.open_tasks", "label": "Open tasks", "format": "int" },
        {
          "type": "table",
          "query": "sample.recent_time_entries",
          "columns": [{ "field": "work_date" }, { "field": "person" }, { "field": "hours", "label": "Hours" }],
          "max_rows": 5,
          "open": true
        },
        { "type": "text", "markdown": "Open the [task list](dest:sample.tasks) to see more." }
      ]
    }
  ],
  "tabs": [
    {
      "id": "sample.task_time",
      "origin": "app:sample",
      "label": "Time",
      "slot": "doc.tab",
      "doctype": "task",
      "panel": "sample.task_overview",
      "order": 1
    }
  ]
}
```

A `text` block is a small markdown subset: paragraphs, emphasis, strong, lists, inline code and links of the
form a link whose target is `dest:` followed by the destination name to registered destinations. HTML, images and any other link are
`uiext.raw_html`:

<!-- ui-example: invalid code=uiext.raw_html file=ui.json -->
```json
{
  "contract_version": "0.1.0",
  "requires_shell": "0.1.0",
  "app": "sample",
  "panels": [
    {
      "id": "sample.overview",
      "origin": "app:sample",
      "label": "Overview",
      "slot": "doc.sidepanel",
      "doctype": "project",
      "blocks": [{ "type": "text", "markdown": "<b>hello</b>" }]
    }
  ]
}
```

A data block without a query is `uiext.query_missing`:

<!-- ui-example: invalid code=uiext.query_missing file=ui.json -->
```json
{
  "contract_version": "0.1.0",
  "requires_shell": "0.1.0",
  "app": "sample",
  "panels": [
    {
      "id": "sample.overview",
      "origin": "app:sample",
      "label": "Overview",
      "slot": "doc.sidepanel",
      "doctype": "project",
      "blocks": [{ "type": "stat", "label": "Open tasks", "format": "int" }]
    }
  ]
}
```

a query your app does not declare is `uiext.unknown_query`:

<!-- ui-example: invalid code=uiext.unknown_query file=ui.json -->
```json
{
  "contract_version": "0.1.0",
  "requires_shell": "0.1.0",
  "app": "sample",
  "panels": [
    {
      "id": "sample.overview",
      "origin": "app:sample",
      "label": "Overview",
      "slot": "doc.sidepanel",
      "doctype": "project",
      "blocks": [{ "type": "stat", "query": "sample.no_such_query", "label": "Open tasks", "format": "int" }]
    }
  ]
}
```

and a query of the wrong kind (a `table` over an aggregate query) is `uiext.query_kind`:

<!-- ui-example: invalid code=uiext.query_kind file=ui.json -->
```json
{
  "contract_version": "0.1.0",
  "requires_shell": "0.1.0",
  "app": "sample",
  "panels": [
    {
      "id": "sample.overview",
      "origin": "app:sample",
      "label": "Overview",
      "slot": "doc.sidepanel",
      "doctype": "project",
      "blocks": [
        { "type": "table", "query": "sample.open_tasks", "columns": [{ "field": "subject" }], "max_rows": 5 }
      ]
    }
  ]
}
```

A tab must show a panel your own document contributes (`uiext.unknown_panel` otherwise).

A **tile** (`home.tiles`) is a composite component plus its props, each a literal or `{ "query": ... }` over an
aggregate query, and an optional `open` to a destination. See the next section for a complete tile.

## Composite components

A component is a small tree of layout nodes (`row`, `column`, `grid`, `group`, `repeat`, `when`, `use`,
`slot`) and controls: the read-only controls of the catalog (`label`, `heading`, `separator`, `badge`,
`progress`, `number`, `currency`, `date`) and `button`, whose only event is `on_click` naming an action of
your app. A component declares typed `props` (at most 16) and named `slots` (at most 8); inside it,
expressions read the props as `doc.<prop>`. The limits are checked when the component is expanded: depth at
most **8**, at most **500** nodes (a `repeat` counts `max` times its body, and `max` is at most 50), and
the references between components must not form a cycle. A component and a tile that uses it:

<!-- ui-example: valid file=ui.json -->
```json
{
  "contract_version": "0.1.0",
  "requires_shell": "0.1.0",
  "app": "sample",
  "components": [
    {
      "id": "sample.project_health",
      "origin": "app:sample",
      "props": [
        { "name": "open", "type": "int", "required": true },
        { "name": "total", "type": "int", "required": true }
      ],
      "slots": ["footer"],
      "body": {
        "node": "column",
        "title": "Project health",
        "children": [
          { "node": "control", "control": "heading", "text": "Project health" },
          {
            "node": "when",
            "test": "doc.total > 0",
            "then": { "node": "control", "control": "progress", "value": "doc.total - doc.open" }
          },
          { "node": "control", "control": "number", "value": "doc.open" },
          { "node": "slot", "name": "footer" }
        ]
      }
    }
  ],
  "tiles": [
    {
      "id": "sample.health_tile",
      "origin": "app:sample",
      "label": "Project health",
      "icon": "chart",
      "slot": "home.tiles",
      "order": 5,
      "component": "sample.project_health",
      "props": {
        "open": { "query": "sample.open_tasks" },
        "total": { "query": "sample.task_count" }
      },
      "open": { "destination": "sample.projects" }
    }
  ]
}
```

Two components that use each other are `uiext.component_cycle`:

<!-- ui-example: invalid code=uiext.component_cycle file=ui.json -->
```json
{
  "contract_version": "0.1.0",
  "requires_shell": "0.1.0",
  "app": "sample",
  "components": [
    { "id": "sample.a", "origin": "app:sample", "props": [], "body": { "node": "use", "component": "sample.b" } },
    { "id": "sample.b", "origin": "app:sample", "props": [], "body": { "node": "use", "component": "sample.a" } }
  ]
}
```

A nesting deeper than 8 is `uiext.component_too_deep`, more than 500 nodes `uiext.component_too_large`, and a
tile or `use` that names a component nobody contributes is `uiext.unknown_component`; a `button` whose
`on_click` names an action your app does not contribute is `uiext.unknown_action`.

## Visibility and enablement

`visible_if` hides an item and `enabled_if` disables it while the expression is false; both are expressions
of the shared language over `doc` (the document being edited) and see a missing field as `null`, never as an
error. `doc.status == 'Draft'` shows an item only on a draft; `not is_empty(doc.assignee)` enables it only
when an assignee is set:

<!-- ui-example: valid file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.start_work"
origin = "app:sample"
label = "Start work"
slot = "doc.toolbar"
doctype = "task"
visible_if = "doc.status == 'Draft'"
enabled_if = "not is_empty(doc.assignee)"
navigate_url = "/sample/reports"
```

An expression that does not parse is `uiext.expression_invalid`, with the parser's message and offset:

<!-- ui-example: invalid code=uiext.expression_invalid file=ui.toml -->
```toml
contract_version = "0.1.0"
requires_shell = "0.1.0"
app = "sample"

[[actions]]
id = "sample.start_work"
origin = "app:sample"
label = "Start work"
slot = "doc.toolbar"
doctype = "task"
visible_if = "doc.status =="
navigate_url = "/sample/reports"
```

**Display is never authorisation** (ADR-069, section 7). `visible_if`, `enabled_if` and `permission` only
decide what the client shows. What is allowed is decided again on the server every time: a `call` runs under
the signed-in user's own rights, a `table` or `stat` shows only what that user's query returns, and the
resolved bundle a user receives is filtered by their permissions for display only
(`Resolved::for_permissions` in `crates/framework/spot-ui-ext`). Hiding a button never protects a method;
declare the method's permission in the manifest.

## Refusals

The installer refuses a document with any finding and loads nothing; each finding names the app, the file,
the item id and the JSON pointer, then one of these codes (`contracts/ui-ext/errors.md`, and the catalogue
`crates/framework/spot-errors/catalogue`).

| Code | What you did wrong | The fix |
| --- | --- | --- |
| `uiext.schema` | The document does not fit the schema in a way no other code names: an unknown property, a wrong type, an unknown control, a missing required property. | Fix the named property; objects are closed, so nothing may be added. |
| `uiext.unknown_slot` | `slot` is not in the closed slot list. | Use a slot from the table above; a new slot needs a contract version bump. |
| `uiext.slot_kind_mismatch` | The item kind is not what the slot accepts (an action in `doc.sidepanel`). | Put the item in a slot that accepts its kind. |
| `uiext.doctype_required` | An item in a `doc.*` or `list.*` slot names no `doctype`. | Add the `doctype`. |
| `uiext.slot_limit` | More items than the slot allows per app and DocType. | Remove items or group them in a panel. |
| `uiext.label_too_long` | A literal label is longer than the slot's label maximum. | Shorten it, or use a `key:` translation key. |
| `uiext.unknown_icon` | `icon` is not a token of the closed set. | Use a token from the contract's icon list. |
| `uiext.effect_invalid` | An action has none, or more than one, of `call`, `open`, `navigate_url`. | Keep exactly one effect. |
| `uiext.external_url` | `navigate_url` has a scheme or host, or is not an absolute path. | Use `open` to a destination, or a path under a declared route. |
| `uiext.unknown_route` | `navigate_url` is not under a route your manifest declares. | Declare the route in the manifest, or use `open`. |
| `uiext.unknown_method` | `call.method` is not declared in the manifest for `call.doctype`. | Declare the method and its arguments in the manifest. |
| `uiext.unknown_destination` | A destination (in `open`, a link, a tile or a `dest:` link) is not in the registry. | Declare the destination or fix its id. |
| `uiext.unknown_permission` | `permission` does not name an existing `<doctype>:<right>`. | Use an existing permission reference; display never grants access. |
| `uiext.expression_invalid` | An expression does not parse, or reads a field its scope does not have. | Fix the expression using the expression language's grammar and the DocType's (or the component's props') fields. |
| `uiext.raw_html` | A `text` block contains markup, an image, or a link that is not `dest:`. | Use paragraphs, emphasis, lists, inline code and `dest:` links only. |
| `uiext.query_missing` | A `table` or `stat` block names no query. | Declare a server query and name it. |
| `uiext.unknown_query` | A block or a tile prop names a query the manifest does not declare. | Declare the query in the manifest, or fix its id. |
| `uiext.query_kind` | A `table` names an aggregate query, or a `stat` a rows query. | Name a rows query for a table and an aggregate query for a stat. |
| `uiext.unknown_panel` | A tab names a panel your app does not contribute. | Contribute the panel in the same app. |
| `uiext.unknown_component` | A tile or a `use` node names a component no installed app contributes. | Contribute the component, or declare the dependency on the app that does. |
| `uiext.unknown_action` | A control's `on_click` names an action your app does not contribute. | Contribute the action in the same app. |
| `uiext.origin_mismatch` | An item's `origin` or id namespace is not your app. | Namespace every id by the app that contributes it. |
| `uiext.duplicate_id` | Two items (of any kind) share an id. | Make every item id unique. |
| `uiext.component_cycle` | Components reference each other in a cycle. | Remove the cycle from the component references. |
| `uiext.component_too_deep` | An expansion nests deeper than 8. | Flatten the component body or split it. |
| `uiext.component_too_large` | An expansion has more than 500 nodes. | Split the component or lower the `repeat` maximum. |
| `uiext.extension_without_dependency` | Your app refers to another app's item without declaring the dependency. | Declare the dependency on the app whose item is used. |
| `uiext.dependency_cycle` | The declared app dependencies form a cycle, so no dependency order exists. | Remove the cycle from the apps' declared dependencies. |
| `uiext.document_too_large` | The document is larger than the size cap and was not parsed. | Split the document into several files of at most 1 MiB each. |

The three last codes are about the apps of a site as a whole (their dependencies and the document size), not
one document's content, so this page shows no single-file example for them.

## Versioning

A document names the `contract_version` it was written for (today `0.1.0`) and `requires_shell`, the
minimum client shell that can render it. A client older than `requires_shell` keeps the last bundle it had
and shows an upgrade notice; a client that meets an item it does not know (an unknown slot or node kind, or
a newer `contract_version`) treats that item as absent and shows the notice, never a partial or guessed
rendering. Every change of the contract is an ADR amendment plus a version bump, recorded in
`contracts/ui-ext/CHANGELOG.md`, and a new slot always needs one; the version the validator knows is the one
this page describes, so when it moves, re-read the changelog and run your document against the new
validator before you ship.
