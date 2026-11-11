# spot-postgres

The PostgreSQL backend of the storage contract (`contracts/storage`, ADR-053). PostgreSQL **18** only
(architecture decision of 29 Sep 2026); `PgBackend::connect` refuses other versions and servers
without ICU.

## What is here (WP-STO-01.T1)

| Area | Where |
| --- | --- |
| Config, pool, `PgBackend`, health | `src/config.rs`, `src/conn.rs`, `src/backend.rs` |
| `PgTransaction`: isolation, timeouts, savepoints, poisoning, commit outcome, drop | `src/tx.rs` (the rules are in its module docs) |
| `PgTransaction`'s raw-SQL seam for sibling `*-pg` crates | `src/tx.rs` (`raw_query`, `raw_query_opt`, `raw_execute`, `raw_execute_with`) |
| SQLSTATE → `ErrorKind` table | `src/error.rs` |
| Test databases | `src/testing.rs` (feature `testing`) |
| Stubs returning `Unsupported`, one directory per later task | `schema/`, `documents/`, `query/`, `locks/`, `sequences/`, `graph/`, `feed/` |

Each trait impl is a one-line delegation to a free function in its area module, so later tasks fill
their module without touching `lib.rs`, `tx.rs` or `Cargo.toml`. Area modules reach the connection
only through `PgTransaction::client()` and record failures with `fail()` / `fail_storage()`, which
implement the poisoning rules.

## The raw-SQL seam (WP-DAG-02.T3)

A sibling `*-pg` crate (DAG run state, BPM cases, edges, jobs, …) owns tables of its own and must
write them **in the caller's transaction**, so its rows and the document engine's rows commit or
roll back together. `PgTransaction` therefore offers a supported seam:

| Method | Use |
| --- | --- |
| `raw_query(sql, params, lock_wait)` | a reading statement; returns every row |
| `raw_query_opt(sql, params, lock_wait)` | a statement that returns zero or one row; more than one is `InvalidQuery` |
| `raw_execute(sql, params)` | a statement that writes at most one record; counts **one** write |
| `raw_execute_with(kind, sql, params)` | states the write count: `PgTransaction::READ` counts nothing, `PgTransaction::write(n)` counts `n` |

The rules, all enforced on the same path as the area modules:

- **One connection.** Every statement runs on the transaction's own connection; the seam never
  checks out a second one, sees the transaction's uncommitted work, and commits or rolls back with
  it. A caller that must not open a second transaction (DAG `apply`, a Case beside a document
  write) passes its `&mut PgTransaction` down instead.
- **Bound parameters, constant SQL.** Values reach SQL only as bound parameters (`$1`, …). SQL text
  is the calling crate's own constant; the crate never builds SQL from values or parses it
  (ADR-45). The seam is not a `run any string` path.
- **Write counting.** The caller declares what the statement writes (`PgTransaction::write(n)`)
  rather than the crate guessing from the text. A `Write` counts before the statement is sent, so
  a refused write reaches no server; `TransactionTooLarge` poisons the transaction; a
  `rollback_to` an earlier savepoint gives the writes it undid back.
- **Read-only.** A `Write` in a read-only transaction fails with `ReadOnlyTransaction` and sends
  no SQL.
- **Errors and poisoning.** Failures map through the same SQLSTATE table as the area modules: a
  recoverable kind aborts the transaction until an earlier savepoint is rolled back, any other
  kind poisons it for good, and in both states the next call sends no SQL.

### Tenancy and tables

- **Tables come from the owning crate's schema setup, never from the hot path.** A sibling crate
  creates its own tables through `spot-pg-migrate` where the document schema is migrated, or
  through its own idempotent `prepare` (the convention `spot-jobs-pg`, `spot-coordinator-pg` and
  `spot-dag-pg` follow), and never touches the document tables. The seam writes rows, not schema.
- **The caller owns its tenant scoping.** One tenant is one database (ADR-34), so the seam adds no
  tenant column and no filter of its own. A crate whose table carries a tenant key stamps and
  filters it itself, reading the tenant from the catalogue and the transaction options in the same
  way the area modules do; `PgTransaction::tenant()` returns the tenant this transaction runs
  against (the database name) for that purpose. A crate that forgets the filter has bypassed
  tenancy, and the seam will not catch it.

## Running the tests locally

The integration tests need a real server. Start one and point the tests at an **admin** URL; every
test creates and drops its own `spot_test_<uuid>` database.

```sh
podman run -d --name spot-pg -e POSTGRES_USER=spot -e POSTGRES_PASSWORD=spot -p 5432:5432 docker.io/library/postgres:18
export SPOT_TEST_PG_URL=postgres://spot:spot@localhost:5432/postgres
cargo test -p spot-postgres
```

Without `SPOT_TEST_PG_URL` the tests print `SKIPPED: set SPOT_TEST_PG_URL` and pass. With
`SPOT_REQUIRE_PG=1` (set in CI on Linux) a missing URL fails them. Test databases left behind by a
crashed run are dropped by the next run once they are an hour old.

## Geo, range and exclusion (ADR-087, WP-STO-03.T7)

`FieldType::Geo` is `public.geometry(Point,4326)` (written through `ST_GeomFromText` and read with
`ST_AsText(.., 7)`, so no float is involved); `FieldType::Range` is `int8range`, `numrange`,
`daterange` or `tstzrange`, sent in binary form and always half-open. Declared exclusions become
`EXCLUDE USING gist (scope WITH =, range WITH &&)` named `spot_<table>_<name>_excl`, and a
conflict is `ErrorKind::ExclusionViolation` (SQLSTATE `23P01`) naming entity, constraint and field
but never the other record. GiST and GIN indexes use the `_gix` and `_ginx` names that
`spot-pg-migrate` plans. `prepare` never creates an extension: a catalog that needs `postgis`,
`btree_gist` or `pg_trgm` and finds it missing fails with `InvalidSchema` naming the extension and
`extension.install`. Search stays `Unsupported` (`Capabilities::search` is false) until its
projection exists (WP-STO-13.T1).
