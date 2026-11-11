# spot-environments-pg

The PostgreSQL `EnvironmentStore` of `spot-environments` (WP-ALM-02.T2, ADR-082 class C). One row
per environment set in `spot_control.spot_environment_set` (`name` primary key, `version bigint`,
`doc jsonb`). No extension is needed. The promotion chain is not stored here.

## Where it may be used

The trait is synchronous, so the store blocks on the shared `spot-pg-bridge`. That is
legitimate in **a control-plane process, a CLI command or a job worker**. It must **never** be
called from a request handler (the bridge refuses with `pg_bridge.wrong_thread` on any tokio
runtime thread, and the store returns `environments.store_failed` naming it) and never inside a
transaction an async store holds (it takes its own pooled connection; holding one while waiting
for another can exhaust the pool).

## Use

```rust,ignore
let store = PgEnvironmentStore::new(bridge.clone(), &backend.pool())?; // plain thread
store.ensure_schema()?; // once at start-up; idempotent, under an advisory lock
let stored = store.create(set)?;
```

`new` refuses a bridge configuration that does not fit the pool and a current-thread runtime.

## Semantics

`create` (version 1, `store_name_taken`), `get` (`store_not_found`), `replace` (compare-and-swap
on the version in one transaction; `store_not_found` and `store_version_conflict` told apart),
`list` (by name, pages of `LIST_PAGE`=100 in one snapshot; above `LIST_CEILING`=5000 sets it
fails with `environments.store_failed`, never truncating). `validate()` runs before every write.
A document that no longer describes a valid set is refused with `environments_pg.row_invalid`.
Deadlines: 5 s read, 10 s write, 20 s list. A timed-out write may or may not have been applied.
Errors carry only a bridge code or SQLSTATE: no SQL, set name, document or connection string.

## Sizing

Sets are few and written by operators, so the load is a handful of short statements. Use
`max_in_flight = 2`, `queue_capacity = 8` and a pool of 4 (the bridge needs
`max_in_flight <= pool - 2`); the pool is shared with the other class C stores of the process,
so add their `max_in_flight` to the pool size rather than raising this one.

## Tests

Need `SPOT_TEST_PG_URL` (admin URL); skipped with a message when unset.
