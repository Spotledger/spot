Part of [Running Spot](../running-spot.md).

# The settings management service

An operator guide for `services/settings_api.rs` (WP-ADM-07.T2). It serves the typed settings schema
of `spot-settings` through the management API, with a value store and change history in PostgreSQL
(`spot-settings-store-pg`).

Enable it with `[services.settings_api] enabled = true` **after** the management API
(`[services.mgmt_api]`); it adds operations to that API's handlers port. It is off by default and
fails the site start when enabled without the management API.

## Operations

| Operation | Risk | What it does |
| --- | --- | --- |
| `settings.list` | read | the settings and values at one scope (`scope`, optional `area`) |
| `settings.get` | read | the effective value across the scope's inheritance chain, and where it came from |
| `settings.set` | medium | sets one value; refuses a high-risk setting |
| `settings.set_high_risk` | critical | sets one high-risk value; the management API demands a signed envelope |
| `settings.history` | read | who, when, channel and the SHA-256 digests of the old and new value |

A scope is `fleet`, `group:<id>`, `server:<id>`, `tenant:<site>`, `company:<site>/<id>` or
`user:<site>/<id>`; omitted, it is this site's tenant scope.

## Rules

- A value is validated against the setting's type and bounds; an unknown key, a read-only setting and
  a scope the setting does not allow are refused. Tenant, company and user scopes must belong to this
  site.
- `expected_version` (0 for "not set") makes a write compare-and-set: a mismatch is
  `settings_api.version_conflict` and nothing is written.
- A setting that needs a restart is applied whole and the result names what must restart.
- A secret setting holds the **name** of an entry in the secret store (lower-case letters, digits,
  `.`, `_`, `-`); `get` and `list` return that name, and the history holds digests only.
