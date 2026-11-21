Part of [Running Spot](../running-spot.md).

# API scripts and scheduled scripts

The optional `scripting` app (`apps/scripting/`) holds two DocTypes. Install the app on a site to
use them; a site without it gets nothing from this module.

| DocType | What it does | Runs as | Entry point |
| --- | --- | --- | --- |
| `api_script` | Serves `POST`/`GET /api/v1/method/scripting/<module>/<function>` | the caller, with the caller's rights; never a guest | `export function on_call({ args, ctx })`, returns JSON |
| `scheduled_script` | Runs on a cron schedule (six fields, seconds first, in the `time_zone`) | the user in `run_as`; if that user is missing or disabled the run is refused (`serverscript.run_as_unavailable`) | `export function on_run({ ctx })` |

`ctx` of a scheduled run has `schedule`, `scheduled_for` (UTC, RFC 3339), `deduplication_key` and
`user`. Neither kind can run as a system principal; elevation is a separate audited flag
(WP-EXT-06.T8) and does not exist for scheduled scripts.

## Who may create them

Only the Tenant Administrator and System Manager roles hold any right on the two DocTypes. A
script can read and write only what its caller (or its `run_as` user) can, so writing one grants
no data access.

## What a script can and cannot do

The same host surface and limits as document-event scripts (`scripts.md`): `host.get`,
`host.insert`, `host.update`, `host.call`, `host.log`, `host.throw`, `host.format_money`,
`host.round`, `host.now`; no network, files, environment or `eval`. The timeouts, memory,
instruction, host-call and output ceilings are the operator's `[services.scripts]` ceilings.
`host.query` is **not available** to these two kinds on this host yet (it answers
`serverscript.store_failed`): the document engine has no permission-filtered query for a given
invocation. A source that does not compile is refused on save with line and column; a source
without its entry point is refused naming it.

## Settings

`[services.script_kinds]` in `common.toml` (unknown keys are refused):

- `enabled` (default `true`);
- `max_scripts` (default 200, 1 to 2,000): of each kind; the rest are skipped and reported;
- `default_rate_limit_per_minute` (default 60): per caller, per script function;
- `refresh_secs` (default 30): how long another node takes to see a change.

## What is live

- **API scripts**: the call after a save, enable, disable or delete commits. A call already
  running finishes with the script it started with.
- **Scheduled scripts**: the schedule set is replaced at the scheduler's next tick (every
  `[services.scheduler] tick_ms`). A run already enqueued runs the script as it is when the job
  executes. A slot never runs twice: its deduplication key is the job's idempotency key and the
  run's command id.
- A change made directly in the database is seen within `refresh_secs`.

## Reading a script's state

`last_error` shows the last failure as a code and a duration (never a message, argument or
value). A scheduled script also keeps `last_run_count`. A scheduled script that fails
`[services.scripts] failure_threshold` times in a row is disabled with `disabled_reason`. API
scripts are not disabled by failures (a caller's bad input would disable a good script) and only
record `last_error`.

## Failure behaviour

- An invalid record is skipped and listed with its reason (`KindsHandle::status`), never a start
  failure.
- Two API scripts claiming one function key are both left out, and the status says so
  (`serverscript.function_conflict`). An API script cannot shadow an app's function.
- A failed scheduled run is not retried by the job engine; the next slot is the next attempt.
- While the engine is unavailable a script run fails closed (`serverscript.store_failed`).
- Logs and audit lines name the script and a stable code, never script text, arguments, results
  or document values.
