# spot-migration-apply-pg

The PostgreSQL wiring of the apply engine (WP-MIG-07.T2, blueprint WS12, ADR-22).

`spot-migration-apply` decides *when* a site closes, which step runs next, when to roll back and
when users may return. This crate supplies the durable PostgreSQL-backed pieces that state
machine runs on: the journal, the maintenance gate, the step runner over `spot-pg-migrate`, the
snapshot port over `spot-snapshot`, the `migration.progress` notifications and the verify gate.

## The control schema

| Table | Holds |
| --- | --- |
| `spot_migration_control.journal` | `(site, seq)` → the write-ahead records |
| `spot_migration_control.gate` | `(site)` → open/closed, reason, since, epoch |
| `spot_migration_control.snapshot` | `(site, snapshot_id)` → the directory a take wrote |
| `spot_migration_control.timing` | one row per snapshot, apply and restore measurement |

The control schema is **outside every tenant table**: `spot-pg-migrate` reads the current schema
(`public`) only, so a migration can never diff, rewrite or drop the journal, and a snapshot
restore of the tenant never touches the record of the restore. `PgApplySchema::prepare` creates
the tables under a transaction-scoped advisory lock, the convention `spot-pg-migrate` and
`spot-coordinator-pg` use.

## Transactional and non-transactional DDL

PostgreSQL has transactional DDL, so a step `spot-pg-migrate` runs in a transaction
(`Run::Tx`, `Run::SetNotNull`, `finalise`) commits or aborts as one unit. The statements
PostgreSQL **cannot** run in a transaction, and how they are handled:

| Statement | Planner run | Handling |
| --- | --- | --- |
| `CREATE INDEX CONCURRENTLY` on an existing table | `Run::Outside` | declared non-transactional; a restore point is recorded and its compensation drops an index a failed build left invalid |
| `DROP INDEX CONCURRENTLY` (cleanup of an invalid index) | `Run::Outside` | declared non-transactional; re-running is safe |
| the batched backfill | `Run::Backfill` | declared non-transactional; batches commit on their own and a rerun continues from the NULLs |

Every step is idempotent because the runner calls `spot_pg_migrate::apply`, which takes the
session advisory lock and re-plans against the catalogue before it acts: a crash leaves the
schema at a point the planner can diff again.

## The gate

`PgAdmissionGate` is one row. Every request path calls `refresh_if_stale` (or `admit`) before
it serves; a cached answer is valid for a short TTL, and a `close`/`open` bumps the row's epoch
so another node re-reads immediately. When a `Coordinator` is wired, `close` and `open` also
advance the cluster-wide counter `migration-gate:<site>`, which is the push invalidation. A
gate whose row cannot be read answers **closed** with the `migration_apply_pg.gate_unreadable`
reason: a request path is never served on an unreadable gate.

## Timings

`PgSnapshotPort` captures take and restore durations; `PgStepRunner` captures one apply duration
per step. Both are integer microseconds from the injected `Clock`'s monotonic reading, and both
are printed by the tests. The 100-branch measurement (WP-MIG-05's request) is `#[ignore]`d:

```bash
SPOT_TEST_PG_URL=postgres://spot:spot@localhost:5432/postgres \
  cargo test -p spot-migration-apply-pg --test measurement -- --ignored --nocapture
```

It prints one `TIMING` line per measurement and a summary:

```text
MEASUREMENT branches=100 snapshot_micros=… apply_micros=… restore_micros=…
```

`SPOT_MEASURE_BRANCHES` scales the branch count. The apply number is dominated by
`spot-pg-migrate` re-reading and re-planning the live schema before every step (its own
resumability contract), so a 100-table plan costs roughly the plan's size per step.

## Tests

```bash
SPOT_TEST_PG_URL=postgres://spot:spot@localhost:5432/postgres \
SPOT_REQUIRE_PG=1 cargo test -p spot-migration-apply-pg
```

Without `SPOT_TEST_PG_URL` the PostgreSQL tests print `SKIPPED` and pass. The kill test spawns
the test binary again as a child process, `SIGKILL`s it at a randomised step boundary and runs
recovery in the parent: the acceptance is that the site ends **new and verified** or **old and
restored**, never open-unverified, and that a probe request path was never served while the gate
was closed.
