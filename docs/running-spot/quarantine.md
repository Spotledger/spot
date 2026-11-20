Part of [Running Spot](../running-spot.md).

# quarantine

Administration of the quarantine queue: list, inspect, retry, release, discard; and the drift
report. Rules: a discard needs the permission `sync.quarantine.discard` and a reason code and is
refused if the permission engine fails; every action writes its `sys_sync_decision` record in the
same transaction as the state change (see below for what is and is not one transaction); a page is at most 100 items; items of DocTypes you cannot
read are counted, not listed; fields you cannot read show as `hidden`; payload views are capped
at 1 MiB. Readiness is information only: `quarantine: 3 items (oldest 2d)`.

## What is wired (WP-RTB-03.T9)

- **Store**: the library's `QuarantineStore` (PostgreSQL in production) behind `StoreBackend`;
  a store that cannot answer is `quarantine_host.unavailable`, never an empty queue.
- **Discard**: `EngineCloseGuard` asks the permission engine for the named permission
  (`sync.quarantine.discard`, spelled `sync_quarantine_discard` in the engine, which allows only
  lowercase letters, digits and `_`); the store asks again, on the quarantined record's own
  DocType, before it closes. An actor with no known principal, an undeclared permission and an
  engine error are all refusals.
- **Order**: the decision is audited first, then the state changes; a discard closes with the
  library's closure record (who, when) written with the state in one statement. A failed audit
  changes nothing. The audit sink is a separate seam from the store, so the audit entry and the
  state change are not one database transaction.
- **Reading**: `FieldGuardScope` shows a DocType only when `read` is allowed and a field only
  when the field guard lists it; a guard error hides the field. A binary image is not returned.
- **Condition**: `QUARANTINED` (Red) is raised for each held item and cleared when it leaves the
  held state; parked dependents and items under an automatic retry are not raised.
- **Readiness**: `quarantine: 3 items (oldest 2d), lag 0.2s, drift 0`; lag and drift appear only
  when their ports answer, and none of it ever makes the site not ready.
