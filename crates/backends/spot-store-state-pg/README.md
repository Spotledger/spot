# spot-store-state-pg

The PostgreSQL implementation of `StoreStateStore` from `spot-store-state` (WP-STR-03.T4,
ADR-082 class C): tables `spot_control.spot_store_state_*`, created idempotently by `prepare()`
under an advisory lock.

## Where it may be used

- **From dedicated plain OS worker threads of a control-plane process.** Never from a tokio
  runtime thread (`spawn_blocking` threads count as inside the runtime): the shared bridge
  refuses with `pg_bridge.wrong_thread`, returned as `store_state.backend`, and nothing changes.
- **Pool and bridge sizing** as for `spot-registry-pg`: `max_in_flight` at least one below the pool
  size minus the bridge reserve (checked by `PgStoreState::new`). Share one bridge per process.
- **Bounded:** reads 5 s, writes 10 s, table creation 30 s. A failure is `store_state.backend`
  with a fixed sentence (no SQL, connection string or row data), never a default value.

## Use

```rust,ignore
let bridge = Arc::new(Bridge::for_backend(config, &backend)?);   // one per process
let state = PgStoreState::new(Arc::clone(&bridge), &backend.pool())?;
state.prepare()?;                                                // idempotent
```

## Atomicity

The listing transition is one statement (insert for the start, compare-and-set on the previous
state otherwise). `next_generation` is one `UPDATE .. RETURNING` on a single counter row. An
idempotency claim is `INSERT .. ON CONFLICT DO NOTHING`. A key hash has a unique index. The audit
table refuses `UPDATE` and `DELETE` with a trigger.

## Tests

`SPOT_TEST_PG_URL=postgres://spot:spot@127.0.0.1:55433/postgres cargo test -p spot-store-state-pg`
(skipped with a message when unset; `SPOT_REQUIRE_PG=1` makes a missing server a failure).
