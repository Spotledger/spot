Part of [Running Spot](../running-spot.md).

# Conditions

A condition (`NODE_SILENT`, `RPO_BREACH`, `CAPTURE_MISMATCH`, ...) is a durable, per-site,
per-subject fact raised by a detector elsewhere in the process (the journal sync job, for
example) and read by health reporting and the Admin Center. The engine itself
(`spot-conditions`) is a pure in-memory state machine: raise, de-duplicate, debounce a clear,
escalate on a schedule, clear. It never reads a clock or a database; the host does both.

## What hosts it

`services/conditions.rs` runs one `ConditionsService` per site: a single task owns a
`ConditionEngine` built from `spot_sync_conditions::catalogue()` plus any definitions other
services add through `ConditionRegistrations::register` before the service starts (the same
pattern `spot-journal-audit-job` uses). A 5-second tick (`services.conditions.tick_seconds`)
drains the transitions the engine's audit sink collected since the last tick, applies them
through `spot-conditions-pg`'s `PgConditionStore`, and lets catalogue-scheduled escalations fire
against the site's own clock.

The two tables are created by `spot migrate` (they are part of the platform's extra tables); the
service never creates one. If it cannot read its open set at start (a table missing, the store
down), it does not start: the site still serves, readiness reports `conditions` as degraded with
the reason, and every call on the handle returns an error, so a dead conditions service never
reads as calm. It is an optional service for that reason: a conditions-store outage does not stop
the site serving.

At start, the service loads the open set from `spot_condition_open` and calls
`ConditionEngine::restore`, which refuses an id the registry does not know or a duplicate
`(id, subject)`; a restart therefore never loses or duplicates an open condition, and raises no
audit event of its own for the conditions it finds already open — restoring is not a transition.

## Store shape

`PgConditionStore::apply` writes, in one transaction, per transition:

- `spot_condition_open`: insert on raise, update in place on escalate, delete on clear. Holds
  exactly the engine's current open set.
- `spot_condition_history`: append-only, one row per transition, bounded to the newest 1,000 rows
  and 90 days per subject (older rows are pruned in the same transaction).
- The outbox: a `sys_condition_raised` event for a raise or an escalation (escalation events
  carry `from`/`to` severity and no new severity field; see the events contract), or a
  `sys_condition_cleared` event for a clear.

A reader never observes one of the three without the other two: a crash mid-write leaves none of
them changed, because all three share the transaction.

## When the store cannot be written

A transition is applied to the in-memory engine at once, regardless of the store; `raise` and
`observe_cleared` do not wait for durability. Writes happen on the tick (and once more at
shutdown), so a transition is durable within `tick_seconds`. If a write to the store fails, the
transitions are kept in a bounded in-memory queue and retried on every later tick, one event at
a time after a failed batch; an event that can never be written (`conditions_pg.corrupt`) is set
aside, counted and logged by id, so it cannot block the ones behind it. Readiness reports
`degraded: <n> transitions waiting ... rejected <r>`. The queue is bounded at 10,000; past
that, the oldest **info**-severity transition is dropped first and the drop is counted — never a
red one. A red transition is never dropped: past 100,000 pending the service reports `failed`
loudly rather than lose it. Once the store starts accepting writes again, the queue drains in
order on the next tick(s) and readiness returns to `conditions: calm`.

## Read port and permissions

`ConditionsView::health(tenant)` returns the `HealthSummary` shape `spot-sync-health` already
consumes, for the Admin Center and management operations to read. It returns an error, not a
calm default, when the service is not running or `tenant` is not the site it serves; the caller
shows "unknown". This service performs no
authorisation of its own: the caller's own permission check (the route or job that asks for the
health summary) is what gates access. The service exposes nothing on HTTP; it is reached only by
other in-process code through `ConditionsHandle`.

## Tenancy

One site, one `ConditionsService`, one database: a condition raised on one site's engine is
never visible to another site's `open()`, `history()` or `health()` — each service only ever
builds a `PgConditionStore` over its own site's backend and catalog.
