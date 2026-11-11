# spot-tenant-move-pg

The PostgreSQL implementation of `MoveStore` from `spot-tenant-move` (WP-CTL-02.T2, ADR-082 class C
item 3). It is the durable record of a tenant move between clusters: one row per tenant in
`spot_control.spot_tenant_move` (revision, the record as `jsonb`, state, source and target cluster,
`updated_at`) and an append-only history in `spot_control.spot_tenant_move_event`, written in the
same transaction as the state change.

## Where it may be used (ADR-082 section 2)

- **From a control-plane process, a CLI command or a job worker. Never from a request handler**, a
  hook, a lifecycle step or a realtime task: the shared bridge refuses such a call with
  `pg_bridge.wrong_thread`, and the store returns it as `tenant_move.store_failed`.
- **Never inside a transaction an async store holds.** Every call takes its own pooled connection
  and its own short transaction.
- **Bounded**: reads use `READ_DEADLINE` (5 s), writes `WRITE_DEADLINE` (10 s), table creation
  `PREPARE_DEADLINE` (30 s). The bridge clamps them to its own `max_deadline`.
- **Visible failure**: a bridge refusal (`busy`, `timeout`, `closed`, `connect`) or a database
  error is `Error::StoreFailed` with a fixed reason. `get` returns `Ok(None)` only when no row
  exists.

## Use

```rust,ignore
let bridge = Arc::new(Bridge::for_backend(config, &backend)?);   // one per process
let store = PgMoveStore::new(Arc::clone(&bridge), &backend.pool())?;
store.prepare()?;                                                // idempotent, advisory-locked
let engine = MoveEngine { store: &store, /* ports */ };
```

`new` refuses (`tenant_move_pg.bad_setup`) a bridge configuration that does not hold against the
pool and a caller inside a current-thread runtime.

## Sizing

A move is driven by one operator or job worker at a time per tenant, and every call is a single
short statement or transaction, so the bridge's `max_in_flight` only has to cover the operators and
workers that share the process (two or three is plenty); the pool must be at least
`max_in_flight + 2` (the bridge reserve plus one spare), which `new` checks. The default bridge
(4 in flight, pool of 6) is enough.

## Semantics

- `create` inserts at revision 0; an existing tenant is `AlreadyExists` (the unique violation is
  recognised by SQLSTATE, not by text).
- `save` is `UPDATE ... WHERE tenant = $1 AND revision = $2`; zero rows is a store failure ("changed
  since it was read", or "disappeared"). The stored document carries the new revision.
- History rows equal the record's own `history`; a record whose history is shorter than the stored
  one is refused and the whole transaction rolled back.
- A stored document that no longer deserialises, or belongs to another tenant, is a store failure.
- No extension is needed (plain PostgreSQL). Every table, index, trigger and function is
  `spot_`-prefixed.

## Tests

`tests/store.rs` needs a server and skips with a message when `SPOT_TEST_PG_URL` is unset (a failure
with `SPOT_REQUIRE_PG=1`). One case table runs against `MemoryMoveStore` and this store.
