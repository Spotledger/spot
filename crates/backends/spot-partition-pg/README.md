# spot-partition-pg

PostgreSQL implementation of `spot_partition::PartitionRegistry` (`PgPartitionRegistry`) and
`spot_partition::RangeAllocator` (`PgRangeAllocator`), over the shared `spot-pg-bridge`
(WP-EDG-03.T8, ADR-082 class C). The in-memory stores are the contract; `tests/parity.rs` runs one
table of cases against both.

## Where it may be used (ADR-082 section 2)

- From a control-plane process (the hub granting ranges and assigning scope keys), a CLI command
  or a job worker. **Never from a request handler**, hook or lifecycle step: on an async thread
  the call is refused with `pg_bridge.wrong_thread` and building the store is refused with
  `partition_pg.unsupported_context`.
- Never inside a transaction an async store holds: it takes its own connection from the pool.
- Every call has a stated deadline (constants in `registry` and `allocator`).

## Sizing

Grants and assignments are rare, short writes. Use `max_in_flight` 2 to 4 and a pool of at least
`max_in_flight + 2` (the bridge reserves one connection and one more stays free). The constructors
refuse a bridge that exceeds `BridgeConfig::largest_in_flight(pool_size)`.

## Guarantees

- `assign` locks the scope key (`pg_advisory_xact_lock`), checks the overlap and inserts the
  assignment and its history row in one transaction: one winner per interval, in any process.
- `lease` locks the series row, advances the `numeric(39,0)` cursor and inserts the grant in one
  transaction: grants are disjoint and gapless, and a rollback changes nothing.
- History and grants are append-only (trigger). No extension is required.
- Failures are `partition.store` with fixed text; the trait reads (`map_at`, `history`, `grants`)
  cannot return an error, so they return an empty answer (fail-closed for ownership) and record
  the failure (`take_read_failure`); use `try_map_at`, `try_history`, `try_grants` for the error.
- Instants are stored as exact UTC nanoseconds; an instant read back is the same moment in UTC.

Tests against a real server need `SPOT_TEST_PG_URL` (skipped with a message when unset, a failure
under `SPOT_REQUIRE_PG=1`).
