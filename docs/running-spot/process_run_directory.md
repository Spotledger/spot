Part of [Running Spot](../running-spot.md).

# Process run directory

The run directory (`crates/framework/spot-cli/src/services/process_run_directory.rs`,
`ORDER = 1140`) is what lets `run_list` answer. The process API
([process_api](process_api.md)) asks a `RunDirectory` port for the ids of the Runs the caller may
see, newest first, and then checks every Run again before showing it. This module provides that
port over `spot_bpm_pg::PgRunDirectory`, which applies the visibility **inside the SQL query**,
before ordering, the page cut and the next-page decision, so a page never leaks a Run and never
comes back short because rows were dropped afterwards.

It starts before `process_api` (`ORDER = 1150`), which looks the port up while the site is
assembled. It is inert when the process engine is not hosted on this site.

## Setting

```toml
[services.process_run_directory]
enabled = true   # the default; false leaves run_list at 503 on purpose
```

Unknown keys are refused at start.

## What it creates

Two indexes on `spot_bpm_case`, created idempotently when the site starts (a failure stops the
start and names the site):

- `spot_bpm_case_listing_idx (tenant, started_at DESC, case_id DESC)`: serves the unfiltered
  listing and the process and status filters;
- `spot_bpm_case_initiator_idx (tenant, initiator, started_at DESC, case_id DESC)`, partial on
  `initiator IS NOT NULL`: serves "my Runs".

No table is added.

## Who sees what

A Run is listed for a person who started it, who is tied to one of its tasks, or who holds
`process.supervise`; `mine=true` narrows to the person's own Runs even for a supervisor. The ids
come from the directory, but `run_get` and the port still check each Run; a directory fault
therefore shows nothing it should not.

## The cursor

`next_cursor` is opaque to callers and tamper-evident. It is `v1.<hex>.<tag>`: the position (the
start instant in microseconds and the Run id) and the first 32 hex characters of a SHA-256 over
the site, the viewer, the process and status filters, `mine`, `sees_all` and that position. A
cursor that does not belong to **this** site, **this** person and **this** filter, a malformed
or oversized (over 256 bytes) one, or one of another version is `400 api.invalid_request`
(`BadCursor`), and the answer never says which. The tag guards against client mistakes and
replay by another person; it is not a secret, and the position it carries is not confidential.

## A 503 from `run_list`

`503 server.unavailable` now means exactly one of: the module is disabled
(`enabled = false`), or the store cannot answer (the database is unreachable). The error never
carries SQL or driver text. Everything else in the process API keeps working in both cases.

## Text for process_api.md

This task does not own `process_api.md`. Replace its section "Listing needs a directory" with:

> ## Listing needs a directory
>
> `run_list` applies the visibility inside the query, before paging, through a `RunDirectory` port.
> The [run directory](process_run_directory.md) module provides it over PostgreSQL and is on by
> default. **`run_list` answers `503 server.unavailable` only when that module is disabled
> (`[services.process_run_directory] enabled = false`) or the database cannot answer.** The port
> checks every row again, so a directory fault shows nothing it should not.
