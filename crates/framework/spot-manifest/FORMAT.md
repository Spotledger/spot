# `spot-app.toml` — the app manifest (ADR-17)

One file per app. Everything an app attaches to the platform — hooks, schedules, routes,
fixtures, patches, capabilities — is declared here; nothing is attached outside it. The
platform parses it with `spot_manifest::parse` and reports every problem at once with a
stable `manifest.*` code and a line and column.

Every table except `[app]` is optional. Unknown keys are errors: a typo must not silently
drop a hook. Defaults are listed per key; a budget is always a whole number of
milliseconds (`budget_ms`) — never a float.

## `[app]` — identity (required)

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `name` | storage-style name (lowercase letters, digits, `_`, starts with a letter) | — | `"erp_selling"` |
| `version` | semver version | — | `"0.3.1"` |
| `title` | string | `""` | `"Selling"` |
| `requires_host` | semver requirement on the platform | absent (any) | `">=0.1, <0.2"` |
| `depends` | list of `{ app = name, version = requirement }` | `[]` | `[{ app = "spot_foundation", version = "^0.2" }]` |

Every `step` elsewhere in the manifest is `<app>:<step>` with this app's `name`; only an
override `target` may name another app's step (`manifest.foreign_step`).

## `[[events]]` — document lifecycle (ADR-51)

One row subscribes one of the app's steps to one hook point of one DocType.

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `doctype` | DocType label | — | `"Sales Invoice"` |
| `event` | ADR-51 hook-point name (see below) | — | `"validate_save"` |
| `step` | `<app>:<step>` | — | `"erp_selling:check_credit_limit"` |
| `priority` | i32 (lower runs first) | `0` | `500` |
| `budget_ms` | integer ms, 1..60000 | `200` | `200` |

Hook-point names are `{before,validate,after}_{create,update,post,cancel,delete,rename,
update_posted}` and `{before,validate,after}_save` (save fires on create and update); the
committed phase suffixes instead of prefixing (`create_committed`, `post_committed`, …);
plus `load` and `field_changed:<field>` with a storage-style field name. A name must
round-trip exactly, so `committed_create` is rejected with `manifest.phase_rule`.

## `[[session_events]]` — session lifecycle

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `event` | `login` \| `logout` \| `session_created` | — | `"login"` |
| `step` | `<app>:<step>` | — | `"erp_selling:warm_price_cache"` |
| `budget_ms` | integer ms, 1..60000 | `200` | `100` |

## `[[boot]]` — boot data contributions

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `step` | `<app>:<step>` | — | `"erp_selling:boot_defaults"` |
| `budget_ms` | integer ms, 1..60000 when stated | absent — the host's boot budget applies | `1000` |

## `[[schedules]]` — recurring jobs

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `name` | string, unique in the manifest | — | `"reprice_open_orders"` |
| `cron` | 6-field cron, seconds first | — | `"0 */15 * * * *"` |
| `step` | `<app>:<step>` | — | `"erp_selling:reprice_open_orders"` |
| `queue` | `default` \| `short` \| `long` | `"default"` | `"long"` |
| `budget_ms` | integer ms, 1..3600000 | `600000` | `600000` |

Cron fields are `second minute hour day-of-month month day-of-week`. Each is `*`, a
number, a `lo-hi` range, or a comma list of those, any of them with a `/step`.
Day of week accepts 0-7, where 7 means the same day as 0. Checks are syntactic; the
scheduler (WP-PLT-07) decides when an entry fires.

## `[[overrides]]` — changing another app's step (ADR-08)

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `target` | `<app>:<step>`, usually another app's | — | `"erp_accounts:post_gl"` |
| `kind` | `before` \| `after` \| `wrap` \| `replace` \| `disable` | — | `"wrap"` |
| `by` | `<this-app>:<step>` | — | `"erp_selling:post_gl_with_commission"` |
| `reason` | string | `""` | `"Commission accrual on posting"` |

`by` is required for every kind except `disable`, and forbidden with it
(`manifest.override_missing_by` / `manifest.override_with_by`).

## `[[seams]]` — service seams (ADR-08, WP-EXT-02)

Each row is exactly one of:

| Key | Type | Example |
| --- | --- | --- |
| `provide` | inline table `{ name, version }` | `{ name = "tax.calculate", version = 1 }` |
| `replace` | inline table `{ name, version, reason }` | `{ name = "erp_accounts:report_render", version = 2, reason = "Commission rows" }` |

A seam name is a dotted name of storage-style parts (`tax.calculate`) or `app:seam`.
`version` is a whole number starting at 1. `replace` must state a `reason`.

## `[[methods]]` — whitelisted instance methods (ADR-16)

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `doctype` | DocType label | — | `"Sales Invoice"` |
| `name` | storage-style method name | — | `"make_payment"` |
| `mutates` | boolean | `false` | `true` |

## `[[functions]]` — whitelisted module-level functions (ADR-16)

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `module` | storage-style module name | — | `"pricing"` |
| `name` | storage-style function name | — | `"quote_price"` |
| `http` | list of `GET` `POST` `PUT` `PATCH` `DELETE` `HEAD` `OPTIONS` | absent (not an HTTP endpoint) | `["GET", "POST"]` |
| `guest` | boolean | `false` | `false` |

## `[[fixtures]]` — records loaded at install

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `doctype` | DocType label | — | `"Role"` |
| `file` | app-relative path (see Paths) | — | `"fixtures/roles.json"` |
| `step` | `<app>:<step>` that imports the records | absent (plain insert) | `"erp_selling:load_roles"` |

## `[[patches]]` — migrations (ADR-22)

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `id` | string, unique, ascending in the order listed | — | `"2026_09_rename_price_list_field"` |
| `file` | app-relative path | — | `"patches/2026_09_rename_price_list_field.toml"` |
| `intent` | `schema` \| `data` \| `metadata` | — | `"data"` |
| `reversible` | boolean | `false` | `false` |
| `idempotent` | boolean | `false` | `true` |

Patches run in the order listed; an id that sorts before the one listed before it is
rejected (`manifest.patch_order`).

## `[[routes]]` — HTTP endpoints

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `path` | route path: starts with `/`, no empty/`.`/`..` segment, no `\` | — | `"/portal/orders"` |
| `step` | `<app>:<step>` | — | `"erp_selling:portal_orders"` |

## `[[assets]]` — static files

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `path` | app-relative path | — | `"assets/selling.css"` |

## `[capabilities]` — what the app may touch

Keys not listed default to the least access.

| Key | Type | Default | Example |
| --- | --- | --- | --- |
| `network` | list of `host:port` endpoints (port 1-65535) | `[]` | `["api.fbr.gov.pk:443"]` |
| `email` | boolean | `false` | `true` |
| `files` | `none` \| `read` \| `read_write` | `"none"` | `"read_write"` |
| `doctypes` | table `{ read = [labels], write = [labels] }` | empty | `{ read = ["Customer"], write = ["Sales Invoice"] }` |

## Paths

File paths (`fixtures`/`patches`/`assets`) are relative to the app crate: `/` separators,
no leading `/`, no `:` (absolute on Windows), no `\`, and no empty, `.` or `..` segment,
so they can never escape the app directory (`manifest.invalid_path`). Route paths
(`[[routes]]`) must start with `/` and have no empty, `.` or `..` segment
(`manifest.invalid_route`).

## Error codes

`parse` returns every problem at once as `ManifestError { code, message, line, col }`;
`manifest.invalid_toml, unknown_key, missing_key, invalid_type, invalid_name,
invalid_version, invalid_event, phase_rule, invalid_session_event, invalid_step,
foreign_step, duplicate_step, budget_out_of_range, duplicate_schedule, invalid_cron,
invalid_queue, invalid_intent, duplicate_patch, patch_order, override_missing_by,
override_with_by, invalid_override, invalid_seam, invalid_path, invalid_route,
invalid_method, invalid_capability`. Codes are the API contract — match on them, never on
the message. `validate` runs the cross-row checks on a manifest built in code.
