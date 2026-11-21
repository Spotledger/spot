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

## `[services.quarantine]`: the backend port (WP-AI-07.T8)

The module provides one port on a site that has a PostgreSQL backend: `Arc<dyn QuarantineBackend>`,
the tenant-scoped store the admin is built on. Before this the port was provided by nobody, so
every consumer answered "no quarantine".

```toml
[services.quarantine]
enabled = true         # the default; false provides no port
provide_store = true   # the default; false provides the backend only (see below)
```

**What it is built from** (all of the site's own):

- the library's `PgQuarantineStore` over the site's pool (its two tables are created on start-up
  by `prepare`, idempotently; a site that cannot prepare them provides no port and logs
  `quarantine_host.unavailable`);
- the site's permission engine, for the discard permission and the DocType right;
- a principal source over the site's role assignments. The actor is a user record id, spelled
  `<id>`, `user:<id>` or `agent:<id>`; an agent acts with the role set of the user record it is
  registered under, never its owner's. Anything else, a user with no active assignment, and a
  runtime that cannot block are unknown, and an unknown actor can discard nothing;
- the platform audit sink: every decision (retry, release, discard) is appended to the site's
  tamper-evident audit chain under the scope `quarantine:<site>` with the actor, the item id, the
  apply key and the reason code, never the held image or a field value. **A decision that cannot
  be appended is refused and changes nothing.**

**Who uses the port:**

- the remedy host's quarantine part (`world_access`), which lists held items and retries one as
  the agent (`agent:<id>`); a retry writes its audit entry first;
- the ops signals (`quarantine` count and age);
- the quarantine admin.

**Absence is not a failure.** A site with no database, a disabled module or a store that cannot
be prepared provides nothing; the site starts, readiness says the consumers are degraded, and the
log says why (a code, never a value). A configuration reload provides the backend the first build
made.

### The store port (WP-AI-07.T17)

Beside the backend the module provides `Arc<dyn QuarantineStore>`, the library's own store trait
(insert, item, record, update, close, list, find), from **the same `PgQuarantineStore` instance**
the backend is built over. There is one store per site, never a second instance, so an item put
through the store port is listed by the backend and a decision taken through the backend is seen
through the store port. The module is the only provider of the port; a second module providing it
is a startup error that names both modules (`the port ... is provided by module ... and again by
module ...`).

- **Setting.** `provide_store` (default `true`) on `[services.quarantine]`. A site that wants the
  backend only sets it `false`; the store port is then absent and the edge intake stays unwired
  (see `sync_node`). It has no effect when `enabled = false`, which provides nothing.
- **Same conditions as the backend.** The store port appears when the module is enabled, the site
  has a PostgreSQL backend and `prepare` succeeded. If `prepare` fails, **both ports are absent**
  and the log carries `quarantine_host.unavailable`.
- **Reload.** A configuration reload provides the backend and the store of the first build.
  A reload with `provide_store = false` provides the backend only.
- **Tables.** `PgQuarantineStore::prepare` stays the creator of `spot_quarantine_item` and
  `spot_quarantine_payload` (idempotently, on start-up); the migrator does not own them. The
  site's database role therefore needs DDL rights (CREATE on its schema). Without them the module
  provides nothing, the site starts and readiness says the consumers are degraded.
- **Consumer.** The edge intake (`sync_node_intake`, role `edge`) wraps the port as its quarantine
  adapter: a record the ownership check refuses is held back here instead of being applied. It is
  not a second administration surface: the admin keeps going through the backend port, with its
  guard, its read scope and its audit.
- **Audit.** Decisions taken through the backend are appended to the audit chain under the scope
  `quarantine:<site>` (WP-SEC-06.T9 verifies chains under that scope). The store port itself
  records no decision: only a person's decision through the backend (retry, release, discard) is
  audited.
