Part of [Running Spot](../running-spot.md).

# The process and run API

`spot serve` serves the process and run operations of native API 0.12.0 (`process_list`,
`run_start`, `run_list`, `run_get`, `run_events`, `run_cancel`) through the `process_api` service
module (`crates/framework/spot-cli/src/services/process_api.rs`, start order `1150`, after the
process engine at `1100`). `spot-http` owns the routes and passes the port no user, role or
tenant; the module authenticates the caller again and decides everything else for that caller.

## Settings

```toml
[services.process_api]
enabled = true   # default; false serves no process routes (they answer 404, not served)
```

Unknown keys are refused. The module is inert, with a debug log line, when the process engine is
not hosted on the site. It is not `Required`: a broken module degrades nothing else.

## Who can start what

A process is listed and startable when its **latest published version** has a `manual` or `api`
trigger whose roles admit the caller. A process the caller may not start answers
`process.not_found`, the same as one that does not exist. The input is checked against the
trigger's input schema and refused with `process.input_invalid`: field paths and fixed codes
only, never a value. A trigger that declares no schema accepts any object.

## Business keys and idempotency

The engine's Case table has a unique `(tenant, business_key)` index, so a caller's key is never
passed on as it is. It is namespaced `<process>:<initiator>:bk:<key>`; with no business key the
`Idempotency-Key` header is used as `<process>:<initiator>:ik:<key>`. Consequences:

- a repeat start by the same initiator with the same key is the existing Run, not a second one;
- two users can use the same key text for different Runs, and neither can collide with, or learn
  of, the other's key;
- only a `bk` key is shown back as `business_key`; an idempotency key never is.

## Who can see a Run

The initiator, a person tied to one of the Run's tasks (assignee, claimer or delegate), and a
holder of the platform permission `process.supervise`. Anyone else gets `process.run_not_found`,
as for an id that does not exist. A source that cannot answer (the task store, the permission
snapshot) denies: a task party is then not recognised, and the list narrows to the caller's own
Runs. A Run shows its step states, the `(doctype, id)` pairs of the documents in its Case file
**that the caller may read** (the document engine's own check, as the caller), and a stable
failure code. It never shows variables, node outputs, failure text or a document value.

`run_events` is derived from the stored Case, so the same Case gives the same trail: kind, node,
actor and time, numbered from 1.

## Cancelling

Only the initiator or a `process.supervise` holder may cancel; a person who sees the Run only
through a task gets `permission.denied`. A Run that has ended is `process.run_not_active`. When
the Case file records a compensation for a document the Run touched, the trail ends with
`run.compensation_pending`: the compensation itself belongs to the effects module (WP-BPM-15.T4)
and is not run here.

## Listing needs a directory

`run_list` applies the visibility inside the query, before paging, through a `RunDirectory` port
that a storage module provides (SQL stays in storage crates). **Until a module provides one,
`run_list` answers `503 server.unavailable`.** The port checks every row again, so a directory
fault shows nothing it should not.

## Audit

Target `spot::audit::process`, one line per `run_start` and `run_cancel`, refusals included:
site, acting user (`-` when none could be resolved), operation, the process name or Run id, the
outcome (`ok` or the refusing code) and, for a cancel, the **length** of the reason in
characters. The reason's text, the input and every key are never logged.
