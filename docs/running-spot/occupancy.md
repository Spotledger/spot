Part of [Running Spot](../running-spot.md).

# Booking holds: `[services.occupancy]`

## What it is

The occupancy service lets a site hold and book units (rooms, desks, slots) for date or instant
ranges without ever double-booking (WP-STO-14.T3, ADR-087 section 6). It serves four routes:

| Route | What |
| --- | --- |
| `POST /api/v1/occupancy/occupancy/holds` | claim a unit for `[start, end)`; a tentative hold with a deadline |
| `POST /api/v1/occupancy/occupancy/holds/{id}/confirm` | turn a live hold into a booking |
| `DELETE /api/v1/occupancy/occupancy/holds/{id}` | release a hold or a booking (releasing twice succeeds) |
| `GET /api/v1/occupancy/occupancy/availability` | the free and busy intervals of one unit |

The `{entity}` segment is `occupancy`, the entity the service stores holds in. A hold is a row
of that entity with one exclusion constraint over `(unit, during)`: the database, not the
application, refuses a double booking.

## Who may do what

- **The holder is you.** A claim is always made as the authenticated principal: `user:<id>` for
  a staff user, `consumer:<id>` for a consumer principal. Nothing in a request names another.
- **Confirm and release** are the holder's. A staff user holding the platform permission
  `occupancy_manage` may confirm or release any hold and sees whether an interval is `held` or
  `confirmed`. Everyone else sees `busy` and nothing more. The permission is granted to nobody by
  default: add it to the roles that run the front desk. A consumer never gets it.
- **The conflict answer says nothing** about who holds the interval, until when, or which range.

## Configuration

```toml
[services.occupancy]
enabled = true              # optional; see "When it runs"
default_ttl_seconds = 900   # a hold's lifetime when the request names none
max_ttl_seconds = 3600      # the longest a request may ask for (at most 86400)
sweep_interval = 30         # seconds between sweeper passes
max_span_days = 366         # the longest claimed range
sweep_batch = 200           # most rows one sweeper batch deletes
span = "instants"           # or "dates": whole days at UTC midnight

# Units whose one owning node is a branch edge (see "One owning node"):
[services.occupancy.homes]
"0192f3c4-0000-7000-8000-000000000001" = "branch-07"
```

Unknown keys are refused at start-up with the key named. Every limit is a configurable default,
not a measured value; raise or lower them for the site.

## When it runs

The module needs the site's metadata to declare the `occupancy` entity (an app adds it with
`spot_occupancy::definition("<unit doctype>", Span::Instants)`) and the database to have the
`btree_gist` extension and the exclusion constraint `ex_occupancy_during`.

- With `enabled` unset the module is inert on a site whose metadata has no `occupancy` entity.
- With `enabled = true` the module is expected: a missing entity, extension or constraint
  **degrades** the site. Readiness says `degraded: occupancy (<reason>)`, the occupancy routes
  answer `503 server.unavailable`, and nothing is written. Install the extension in the Admin
  Center database manager (`extension.install`) and run `spot migrate`.
- With `enabled = false` it is off.

The service is **optional**: when it cannot start the site still serves everything else.

## The sweeper

A hold has a deadline; the constraint cannot see the clock, so an expired hold keeps its row
until it is deleted. Two things delete it:

1. **The next claim** takes over an expired hold on the same unit and range in its own
   transaction, so a late or absent sweeper never makes a booking wrong.
2. **The sweeper**, every `sweep_interval` seconds, deletes expired tentative holds in batches
   and emits `occupancy_expired` for each, in the same transaction as the delete.

Exactly one node per site sweeps: the sweeper holds the coordinator lease `occupancy_sweeper`;
the other nodes stand by and retry every ten seconds. A node that crashes mid-batch rolls the
batch back and loses nothing; the next holder of the lease finds the same rows. A confirm that
arrives after the deadline is refused (`409 occupancy.hold_expired`) even when the sweeper has
not run yet.

## One owning node per resource

Every unit has exactly one node that writes its holds: the hub by default, or the one edge
named in `[services.occupancy.homes]`. A claim, confirm or release for a unit this node does not
own answers `403 partition.not_owner` and touches no storage. This node is an edge only when
`[services.node_role] role = "edge"` and `[partition] node` names it.

## Events

`occupancy_held`, `occupancy_confirmed`, `occupancy_cancelled` and `occupancy_expired` are
captured in the transaction that changes the row and published after commit by the outbox. Event
payloads carry the typed holder reference, never a credential.

## Audit

Target `spot::audit::occupancy`: one line per operation with the site, the action, the actor
(the typed holder reference), the unit and record ids and the outcome. Ranges and payloads are
never logged.

## Retries

A claim that fails to serialise is rerun up to eight times inside the service. A caller that
still gets `503` may repeat the request; sending the same `Idempotency-Key` makes a repeated
claim answer the first answer instead of claiming twice.

## Consumers (ADR-083)

A consumer is a separate principal at the edge of the same tenant. A consumer bearer is accepted on
the occupancy routes through the resolver of `services/consumer_session.rs`:

- The module needs a consumer session service (the consumer login service of ADR-083 task 14, **not
  built yet**). Until one provides the `ConsumerSessions` port, the module is inert and a consumer
  bearer is refused like any unknown credential (`auth.invalid_credentials`).
- With the service present, a live session of **this site** whose consumer id follows the holder
  grammar becomes the holder `consumer:<id>`. A session of another site, an invalid id or a missing
  credential is refused.
- A consumer claims for itself, confirms and releases **its own** holds only, and reads only what an
  ordinary caller reads (`busy`, no holder). It never holds `occupancy_manage`. Every other route
  refuses a consumer bearer with `permission.denied` at the audience gate.
- Staff behaviour is unchanged: the staff credential is tried first.
