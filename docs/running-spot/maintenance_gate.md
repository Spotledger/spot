Part of [Running Spot](../running-spot.md).

# The maintenance gate in `spot serve`

`spot migrate-apply` closes the site's admission gate (one row in `spot_migration_control.gate`)
while a migration runs. `spot serve` reads that row and refuses requests while it is closed, so no
user session and no write reaches the database during a schema change (ADR-22).

```toml
[services.maintenance_gate]
enabled    = true   # default
refresh_ms = 1000   # 100 to 30000: how often the gate is read
ttl_ms     = 2000   # never below refresh_ms
```

## What users see

While the gate is closed every request except `GET /api/health/live`, `GET /api/health/ready` and
`GET /metrics` gets `503` with `Retry-After: 30` and the standard error body, code
`server.maintenance`, carrying the request id. The refusal happens before any handler, session check
or database access. Probes still answer, and carry the header `x-spot-maintenance-gate: maintenance_gate:
closed since <unix micros>: <reason>` so an operator sees why.

## Failing closed

A gate that was never read, or whose last read failed, is closed. The site refuses to start when the
first read fails (the service is required). The one open case is a database with **no** control
schema at all (a site older than `new-site` creating it): it has never migrated through the gate and
is served. A schema that exists but cannot be read is closed.

A gate closed because it **cannot be read** (the database gave no connection, or the read failed) answers
`503` `server.unavailable` with the same `Retry-After` header and a `retry_after` detail, the answer a
database with no connection gets everywhere else: no migration closed the site, so it is not reported
as maintenance. Only a gate a migration closed answers `server.maintenance`.

## Closed after a failed apply

If the site stays closed after `spot migrate-apply` failed, run `spot migrate rescue --site <site>`
(see `migrate_rescue.md`) and the apply doc `migrate_apply.md`; the gate is opened again by the
rescue or by the apply's own completion.

## Several nodes

Each node reads the row every `refresh_ms`; a second node sees a close or open within that interval.
The cluster coordinator counter that would make it immediate is not offered to modules as a port yet,
so nodes poll.
