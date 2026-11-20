# Scripting: API and scheduled scripts (WP-EXT-06.T6)

The optional `scripting` app holds the two kinds of Server Script that are not document
events. Document-event scripts live in the platform app (`server_script`); see
`docs/running-spot/scripts.md`.

| DocType | What it is | Runs as | Entry point |
| --- | --- | --- | --- |
| `api_script` | A TypeScript function served as `scripting/<module>/<function>` (`POST`/`GET /api/v1/method/scripting/<module>/<function>`). | The caller, with the caller's rights. Never a guest. | `export function on_call({ args, ctx })` returns JSON |
| `scheduled_script` | A TypeScript function that runs on a cron schedule (six fields, seconds first). | The user in `run_as`. If that user is missing or disabled the run is refused. | `export function on_run({ ctx })` |

Neither kind can run as a system principal. Elevation, where it exists at all, is a separate
audited per-script flag (WP-EXT-06.T8) and is not available to scheduled scripts.

Only the Tenant Administrator and System Manager roles may create, read, write or delete either
DocType (`permissions/grants.toml`). A script runs with its caller's (or its run-as user's)
rights, so writing a script grants no data access of its own.

Both kinds compile on save with the same diagnostics as document-event scripts (line and
column on the source field), run on the same engine with the same limits (200 ms default
timeout, 2 s site maximum, 100 host calls, 256 KiB result), and have the same host surface:
`host.get`, `host.query`, `host.insert`, `host.update`, `host.call`, `host.log`, `host.throw`,
`host.format_money`, `host.round` and `host.now`. There is no network, filesystem or
environment access.

The library is `crates/framework/spot-server-scripts` (`kinds` module). The app declares no
controller or events: the host installs the compile-on-save controllers and serves the scripts
(WP-EXT-06.T7).
