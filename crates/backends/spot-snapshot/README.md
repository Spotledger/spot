# spot-snapshot

Snapshot and restore for Spotledger tenant data: the `SnapshotProvider` trait, the
checksummed `snapshot.toml` manifest (`Snapshot::open` validates it; a changed byte
fails `verify` naming the file), and the two providers that work on every platform:

- **`PgDumpProvider`** — portable, online-consistent. `pg_dump --format=directory`
  inside one exported snapshot (`pg_export_snapshot()`, so the dump is the same
  transactional view while the system runs), restore with `pg_restore --jobs=N` into
  a new database or, with explicit `replace` and the confirmation token, over the
  same name after renaming the old database aside (never dropped by this crate).
  Binary location: the bundled `PgLayout`, else `SPOT_TEST_PG_BIN`, else `PATH`; a
  missing tool is `snapshot.tool_missing`. Passwords ride the child environment
  (`PGPASSFILE`/env) or the connection URL in-process only — never a command line,
  a log, or the manifest.
- **`StoppedInstanceProvider`** — fast, offline. Stops the supervised instance
  through `spot-pg-supervisor`, checksum-copies the data directory (layout
  preserved), restarts the instance even when the copy fails (a guard whose `Drop`
  and explicit `finish` both restart), and restore replaces a stopped instance's
  data directory atomically (rename aside, copy, verify, switch).

After a restore the outbox must start a new generation: wire the `post_restore`
step (it calls `spot_postgres::start_new_generation`), after which old feed
bookmarks fail with `ResyncRequired`, and reset the manifest's node-local keys
(the node id). `restore_and_check` runs registered `InvariantCheck`s against the
restored site and quarantines a failed restore under a clearly named,
non-activated path (`snapshot.invariants_failed`); two no-ERP checks ship
(catalogue version, `spot_*` row counts).

## Error catalogue

Every raised code lives in `crates/framework/spot-errors/catalogue/snapshot.toml`
with its remedy (`snapshot.version_mismatch`, `snapshot.replace_required`,
`snapshot.tool_missing`, `snapshot.invariants_failed`, `snapshot.verify_failed`,
`snapshot.io` …).

## Tests: what runs where

The suite splits by environment; PostgreSQL 18 is the target (a restore into any
other major version is refused, `snapshot.version_mismatch`).

| Tests | Need | Notes |
| --- | --- | --- |
| `tests/manifest.rs`, `tests/stopped.rs` | nothing | run everywhere, including CI runners without any PostgreSQL: the stopped provider's guard/failure paths run against a fake `initdb`/`postgres`/`pg_ctl` bundle (`tests/support/fake_pg.rs`) |
| `tests/pgdump.rs` | `SPOT_TEST_PG_URL` | a real **PostgreSQL 18** admin URL (`postgres://user:pass@host:port/postgres`); without it each test prints one `SKIPPED` line and passes; with `SPOT_REQUIRE_PG=1` and no URL they panic — CI can never pass by skipping. The client tools must be discoverable: `SPOT_TEST_PG_BIN=<layout root>` (its `bin/` holds `pg_dump`/`pg_restore`) or a standard install layout, else `PATH` — a set URL with no tools fails loudly |
| `tests/bench.rs` (`#[ignore]`) | see above | run with `cargo test -p spot-snapshot --test bench -- --ignored --nocapture`; the stopped row runs anywhere, the pg_dump row needs `SPOT_TEST_PG_URL` (skipped otherwise); `SPOT_BENCH_ROWS_PER_BRANCH` scales the fixture |

Set `SPOT_TEST_PG_URL` exactly like `crates/backends/spot-postgres` does (same skip
discipline, mirrored here). The stopped-instance tests never need a bundled layout —
the fake bundle stands in for it.