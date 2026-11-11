# spot-registry-pg

The PostgreSQL implementation of `IndexStore` from `spot-registry` (WP-STR-03.T2, ADR-082 class C
item 2): publishers, ownership, releases, transfers and catalogue search in the control schema
(`spot_control.spot_registry_*`).

## Where it may be used (ADR-082 section 2)

- **From a control-plane process, a CLI command or a job worker. Never from a request handler**, a
  hook, a lifecycle step or a realtime task: the shared bridge refuses such a call with
  `pg_bridge.wrong_thread`, which the store returns as `registry.storage`.
- **Never inside a transaction an async store holds.** Every call takes its own pooled connection
  and its own short transaction.
- **Bounded**: reads `READ_DEADLINE` (5 s), writes `WRITE_DEADLINE` (10 s), search `SEARCH_DEADLINE`
  (10 s), table creation `PREPARE_DEADLINE` (30 s); the bridge clamps them to its `max_deadline`.
- **Visible failure**: a bridge refusal or a database error is `registry.storage` with a fixed
  sentence (no SQL, no connection string, no row data). A read returns `Ok(None)` or an empty list
  only when nothing exists.

## Use

```rust,ignore
let bridge = Arc::new(Bridge::for_backend(config, &backend)?);   // one per process
let index = Arc::new(PgIndexStore::new(Arc::clone(&bridge), &backend.pool())?);
index.prepare()?;                                                // idempotent, advisory-locked
let registry = Registry::new(index, blobs, policy, trust, audit);
```

`new` refuses (`registry_pg.bad_setup`) a bridge configuration that does not hold against the pool
and a caller inside a current-thread runtime.

## Sizing

A registry mutation is one short transaction, and calls for different apps run in parallel while
calls for one app serialise on its advisory lock. A publish pipeline or a CLI uses a handful of
concurrent calls, so a bridge with `max_in_flight` 4 (the default) on a pool of 6 is enough; raise
both together if several publish workers share the process. `new` checks the pair.

## Several registries, one database

Each mutation takes `pg_advisory_xact_lock(hashtextextended(app, 7))`, then repeats the checks of
the in-memory reference under that lock (ownership, publisher status, version rules). The digest
column is unique, so a digest cannot be indexed twice even across apps. Two instances sharing the
database behave as one: one first claim wins, versions only increase, a re-commit returns the stored
release with `inserted = false`, a removal marks once and keeps the first reason, and a transfer
applies only from the signed generation.

## Search

PostgreSQL narrows the candidates (visibility, publisher, category, capability flags, and "shares a
token with the query or is named like it", using a GIN index on `search_tokens`); the contract's
ranking, the latest-release-per-app choice, the host-compatibility filter and the cursor are then
applied in Rust on those rows exactly as `MemoryIndexStore` does, so the order and the cursors are
identical. A `tsvector` column with its own GIN index (built with the `simple` configuration, no
extension) is kept up to date for a later full-text path. Ties are broken by app name.

## Tests

`tests/index.rs` needs a server and skips with a message when `SPOT_TEST_PG_URL` is unset (a failure
with `SPOT_REQUIRE_PG=1`). One case table runs against `MemoryIndexStore` and this store; a fixed
corpus of 200 apps is searched with 23 queries (all pages and cursors) on both and compared.
