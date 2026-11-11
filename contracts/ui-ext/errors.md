# UI extension errors (`uiext.*`)

Stable codes, one per rule. A failing install or save names the app, the file, the item id and the
JSON pointer, then this code. The shared corpus in `examples/invalid/` has at least one file per
code; its `$comment` header says `expect: <code>`. Both implementations (WP-UI-13.T2 validator,
WP-UI-13.T3 renderer) must refuse every file with that code.

| Code | When | Message pattern | Remedy for the app author |
| --- | --- | --- | --- |
| `uiext.schema` | The document does not fit the schema in a way no other code names (unknown property, wrong type, unknown control, missing required property). | `{file}: {pointer}: {detail}` | Fix the property; objects are closed, so nothing may be added. |
| `uiext.unknown_slot` | `slot` is not in the closed slot list. | `slot {slot} does not exist (contract {version})` | Use a slot from `schema/slot.json`; a new slot needs a contract bump. |
| `uiext.slot_kind_mismatch` | The item kind is not what the slot accepts (an action in `doc.sidepanel`). | `{slot} accepts {accepts}, not {kind}` | Put the item in a slot that accepts its kind. |
| `uiext.doctype_required` | An item in a `doc.*` or `list.*` slot has no `doctype`. | `{id} sits in {slot}: name its doctype` | Add `doctype`. |
| `uiext.slot_limit` | More items than the slot allows per app and DocType. | `{app} puts {n} items in {slot}; the limit is {max}` | Remove items or group them in a panel. |
| `uiext.label_too_long` | A literal label is longer than the slot's `label_max`. | `label of {id} has {n} characters; {slot} allows {max}` | Shorten it or use a `key:` translation key. |
| `uiext.unknown_icon` | `icon` is not a token of the closed icon set (a URL, a path, an unknown name). | `icon {value} is not an icon token` | Use a token from `schema/common.json`. |
| `uiext.effect_invalid` | An action has none, or more than one, of `call`, `open`, `navigate_url`. | `{id} must have exactly one effect` | Keep one effect. |
| `uiext.external_url` | `navigate_url` has a scheme or host, or is not an absolute path. | `navigate_url {value} leaves the app` | Use `open` to a destination, or a path under a declared route. |
| `uiext.unknown_route` | `navigate_url` is not under a route the manifest declares. | `no declared route covers {value}` | Declare the route in the manifest, or use `open`. |
| `uiext.unknown_method` | `call.method` is not declared in the manifest for `call.doctype`. | `{doctype}.{method} is not a declared method` | Declare it in the manifest (ADR-16). |
| `uiext.unknown_destination` | A destination (in `open`, a link, a tile, a `dest:` markdown link) is not in the registry. | `destination {id} does not exist` | Declare the destination (ADR-069 section 3). |
| `uiext.unknown_permission` | `permission` does not name an existing `<doctype>:<right>`. | `permission {value} does not exist` | Use an existing reference; display never grants access. |
| `uiext.expression_invalid` | An expression string does not parse in the shared language. | `{pointer}: {parser message} at offset {n}` | Fix the expression (docs/expression-language.md). |
| `uiext.raw_html` | A `text` block contains markup, an image, or a link that is not `dest:`. | `{pointer}: markup is not allowed` | Use the restricted markdown subset. |
| `uiext.query_missing` | A `table` or `stat` block has no `query`. | `{pointer}: a data block names a query` | Declare a server query and name it. |
| `uiext.unknown_query` | A block or a tile prop names a query the manifest does not declare. | `query {id} is not declared` | Declare it. |
| `uiext.query_kind` | A `table` names an aggregate query, or a `stat` a rows query. | `{id} is {kind}; a {block} needs {needed}` | Name a query of the right kind. |
| `uiext.unknown_panel` | A tab names a panel the app does not contribute. | `panel {id} is not in this document` | Contribute the panel. |
| `uiext.unknown_component` | A tile or a `use` node names a component the app does not contribute. | `component {id} is not in this document` | Contribute it. |
| `uiext.unknown_action` | A control's `on_click` names an action the app does not contribute. | `action {id} is not in this document` | Contribute it. |
| `uiext.origin_mismatch` | An item's `origin` or id namespace is not the contributing app. | `{id} is not owned by app:{app}` | Namespace ids by your own app. |
| `uiext.duplicate_id` | Two items (of any kind) share an id. | `{id} is used twice` | Make ids unique. |
| `uiext.component_cycle` | Components reference each other in a cycle. | `component cycle through {id}` | Remove the cycle. |
| `uiext.component_too_deep` | An expansion nests deeper than 8. | `{id} nests {n} deep; the limit is 8` | Flatten it. |
| `uiext.component_too_large` | An expansion has more than 500 nodes (a `repeat` counts `max` times its body). | `{id} expands to {n} nodes; the limit is 500` | Split it or lower `max`. |
