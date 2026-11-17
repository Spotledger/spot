Part of [Running Spot](../running-spot.md).

# Server Scripts and TypeScript controllers

An administrator can add behaviour to a DocType without building an app: a short TypeScript
function in a `Server Script` document that runs when a document is validated, saved, submitted or
cancelled. An app can ship a whole controller in TypeScript (`controllers/<doctype>.ts`). Both are
ordinary steps of the document lifecycle, run on the embedded TypeScript engine as **the user who
triggered the operation**. The host module is
`crates/framework/spot-cli/src/services/scripts.rs`; the machinery is the `spot-server-scripts` and
`spot-ts-controllers` crates.

Scripts are an optional service. If they cannot be loaded the site says so on
`/api/health/ready`; a DocType that can have scripts then **refuses writes** rather than skipping a
validation that has not loaded.

## Write a script

Create a `Server Script` document: the DocType, the event, the TypeScript source, an optional
timeout and priority.

| Field | Meaning |
| --- | --- |
| `name` | The script's name (unique). Also the step it runs as. |
| `doctype` | The DocType whose events trigger it. |
| `event` | `before_validate`, `validate`, `before_save`, `after_save`, `before_submit`, `on_submit`, `before_cancel`, `on_cancel` or `after_commit`. |
| `script` | The source. It exports `on_<event>({ doc, ctx })`. |
| `timeout_ms` | How long it may run; clamped to the operator's maximum, never raised. |
| `priority` | 0 to 9,999: lower runs first within the event. |
| `enabled` | Whether it runs. A script that keeps failing is disabled with a reason. |

```ts
export function on_validate({ doc }) {
  if (doc.total.startsWith("-")) {
    host.throw({
      code: "selling.total_negative",
      message: "The total cannot be negative.",
      fields: [{ field: "total", code: "selling.total_negative", message: "The total cannot be negative." }],
    });
  }
}
```

A `before_*` script may return `{ patch: { field: value } }` to change its own document; any other
event that returns a patch fails with `serverscript.phase_violation`. Money and quantities are
exact strings, never numbers.

**The source is compiled when you save.** A script that does not compile cannot be saved: every
diagnostic comes back as an error on the `script` field with its line and column. Fix it and save
again. The hash of the compiled source is stored in `compiled_hash`.

**A change takes effect on the next request**, on this node at once and on another node within
`refresh_secs`. There is nothing to restart.

### Who may write scripts

Creating, editing, disabling and deleting a `Server Script` needs the right on the `server_script`
DocType. **No role holds it by default**: `apps/platform/permissions/grants.toml` gives none, so only
a Tenant Administrator can. Give it to a role only if that role may run customer code in the server.

A script cannot be attached to a DocType that holds credentials or authorisation (the reserved
list: `api_key`, `api_key_scope`, `linked_identity`, `login_attempt`, `role`, `role_assignment`,
`role_assignment_scope`, `role_duty`, `server_script`, `session`, `user`, `user_recovery_digest`,
`webhook`, `webhook_delivery`). The save is refused with `serverscript.record_invalid` on `doctype`.

## What a script can and cannot do

The host surface is closed: `host.log`, `host.get`, `host.query`, `host.insert`, `host.update`,
`host.call`, `host.throw`, `host.format_money`, `host.round` and `host.now`. There is no network,
no file access, no environment, no `eval`, no `fetch`, no timers.

A script runs as the user who triggered the operation, through the same permission checks as the
API: it sees only the fields that user may read (hidden fields are gone, masked fields are masked)
and its `host.get`, `host.query`, `host.insert`, `host.update` and `host.call` are authorised as
that user. Nothing gives a script the system principal.

## Order

At one hook point the engine runs steps by ascending priority, then by name. **A script runs after
every native step of the same hook point**: its step priority is 1,000,000 plus the event's base
plus the script's own `priority`. Among scripts: `before_validate` before `before_save` (they share
a hook point), then `priority`, then name. A TypeScript controller orders like any controller of its
app.

## Limits

`[services.scripts]` in `common.toml` sets the ceilings. A script can ask for less and never for more.
Unknown keys are refused; an out-of-range value stops the site start and names the key.

```toml
[services.scripts]
enabled = true              # false: no script runs (the log says so)
refresh_secs = 30           # how long another node may take to see a change (1 to 3600)
default_timeout_ms = 200    # when a script names none
max_timeout_ms = 2000       # the most any script may take (1 to 10000)
memory_bytes = 16777216     # per run (1 MiB to 256 MiB)
instructions = 100000000    # per run
host_calls = 100            # per run (1 to 10000)
output_bytes = 262144       # the largest result (1 KiB to 4 MiB)
failure_threshold = 5       # consecutive failures that disable a script (1 to 100)
max_scripts_per_event = 8   # per DocType and event (1 to 32)
log_capacity = 256          # runs kept in memory
```

A script that exceeds a limit (time, memory, instructions, host calls or output) is cut off and
the operation fails with `serverscript.limit_exceeded`, naming the script; the document is not
saved. An `after_commit` script cannot undo a save: its failure is logged and, after
`failure_threshold` consecutive failures, the script is disabled (`enabled` off, the reason in
`disabled_reason`, the code of the last failure in `last_error`).

## Logs and privacy

`host.log` lines and run records are kept in memory and written to the log through `spot-redact`.
An error, log line or audit record names the script and a stable code; **it never carries the
source, a field value or a secret**. A compile diagnostic carries line, column and a message about
the syntax.

## TypeScript controllers

An app whose manifest says `[app] controller = "ts:controllers/invoice.ts"` ships the module in its
own directory. The host reads it from the installed app (only that directory: a path with `..`, a
root, a hidden name or a symlink is refused, and so is a file over 1 MiB), compiles it once per
content, and registers it exactly like a native controller. Its steps run at the hook points it
declares; its whitelisted methods appear in `document_method_call` with the right they declare. It
runs as the triggering user under the same ceilings as a script.

## How the site installs them

At start-up (and at every reload) the site builds its controllers in one pass, in this order:

1. every installed app's `[app] controller` reference: a `native:` reference is looked up in the native
   table of the binary; a `ts:` reference is built by the TypeScript factory from the app's own
   directory on the site's script engine, under the `[services.scripts]` ceilings. A native app is
   never given the TypeScript factory and a TypeScript app is never looked up in the native table;
2. the Server Script controllers: the administration controller of `server_script` and one hub per
   scriptable DocType of the site's metadata (a reserved DocType gets none);
3. the whole set is installed into the platform's strict registry. A module that does not load, a
   controller the binary lacks and a step or method conflict are all reported together and the site
   does not start.

With `[services.scripts] enabled = false` step 2 registers nothing: no hub exists, so no write is
refused as "scripts are not loaded". With scripts enabled, a write to a scriptable DocType is refused
with `serverscript.store_failed` only for the short time between the controllers being installed and
the scripts module finishing its first load.

A DocType created while the site is running gets **no hub until the next build** (a reload or a
restart); scripts attached to it start to run then. Editing a script needs no restart: the fixed steps
of the hub read the current scripts.

## When something is wrong

| What you see | What it means | What to do |
| --- | --- | --- |
| `serverscript.compile_failed` on save | The source has a syntax error. | Read the line and column on the `script` field. |
| `serverscript.limit_exceeded` | A script hit a ceiling. | Make it lighter, or raise the ceiling in `[services.scripts]` if the work is legitimate. |
| `serverscript.phase_violation` | A non-`before_*` script returned a patch. | Derive in `before_*`, check in `validate`. |
| `serverscript.record_invalid` on `doctype` | The DocType is reserved. | Use a different DocType. |
| `serverscript.store_failed` on a write | Scripts are not loaded (start-up) or the store is unavailable. | Wait for start-up; if it persists read `/api/health/ready`. |
| `services.scripts.<key>: out of range` | A ceiling is outside its bounds. | Fix the value in `common.toml`. |
