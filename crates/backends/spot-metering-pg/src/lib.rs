//! The PostgreSQL [`MeterStore`](spot_metering::MeterStore) (WP-CTL-05.T2).
//!
//! `spot-metering` counts what a tenant consumed behind the object-safe `MeterStore` trait and
//! ships only the in-memory store. [`PgMeterStore`] is the durable one: quotas and billing keep
//! their numbers across a restart. It owns five tables created by [`PgMeterStore::prepare`] (see
//! [`schema`]) and passes `spot_metering::contract::run_store_contract`.
//!
//! # Rules the store keeps
//!
//! - **One tenant per store.** `PgMeterStore::new(pool, tenant)` binds the instance; every row it
//!   writes carries that tenant and every filter names it. `add_events` refuses a batch containing
//!   an event of another tenant with `TenantMismatch` before the database is touched, and
//!   `period_totals`/`distinct_members` refuse to answer for another tenant, so a producer cannot
//!   write into, or read, a customer it was not handed (ADR-061).
//! - **Idempotent by `(tenant, event id)`.** `add_events` inserts every event with
//!   `ON CONFLICT DO NOTHING` and only the rows that were really inserted count. Totals and
//!   members are updated **only for those new rows, in the same transaction**, so a retried or
//!   overlapping batch (or two concurrent callers adding the same batch) never counts twice. The
//!   first occurrence of an id inside one batch wins. The return value is the number of newly
//!   recorded events.
//! - **Bounded calls.** One `add_events` call takes at most
//!   `spot_metering::MAX_BATCH_EVENTS` (10 000) events; a larger batch is refused with
//!   `BatchTooLarge` before any write, so no single call holds one transaction for an unbounded
//!   time. A caller with a bigger backlog sends several calls — recording is idempotent, so the
//!   totals are the same.
//! - **All or nothing.** A batch is one transaction. A malformed event or an unresolvable period
//!   refuses the whole batch before the database is touched; a counter sum that would pass
//!   `u64` (the `spot_meter_total_sum_range` constraint) is `CounterOverflow` and rolls the
//!   batch back. A deadlock or serialisation failure runs the (idempotent) transaction again.
//! - **Totals are maintained, not recomputed.** Counters add, gauges keep `GREATEST`, distinct
//!   sets count the member rows really inserted. Reads (`period_totals`, `distinct_members`) use
//!   one repeatable-read snapshot, so a count and its member list agree; members come back
//!   sorted.
//! - **Compaction keeps what retries need, and only that.** `compact(before, policy)` nulls the
//!   raw metadata (meter, amount, member digest, instant) of events in periods that ended at or
//!   before `before.start`, in bounded batches, and keeps the `(tenant, event id)` tombstone, the
//!   period end the tombstone is retained by, every total and every member. The caller's `before`
//!   cut says those periods are final (closed and reconciled).
//! - **The tombstone horizon bounds growth.** With
//!   `RetentionPolicy::tombstone_horizon_periods`, compaction also deletes the tombstones of
//!   periods more than that many periods older than the cut, in bounded batches, after recording
//!   the new floor in `spot_meter_retention`. From then on every event whose period ends at or
//!   before the floor's start is refused with `LateBeyondHorizon` — whether or not the id was ever
//!   seen — so a purged id can never be counted twice. The floor is written before any tombstone
//!   is deleted, and `add_events` creates the tenant's floor row (a no-floor sentinel) and reads
//!   it under a shared row lock in the same transaction, so a crash, a concurrent batch or the
//!   tenant's **first** compaction cannot open a gap between "the id is gone" and "the period is
//!   refused". The floor only moves forward.
//! - **How to size the horizon.** It is the store's late-arrival window: no policy refuses an old
//!   instant on its own, so everything older than the floor is refused. Size it as the longest
//!   producer retry window plus the lag between a period closing and its last reconciliation, plus
//!   one period of slack; `RetentionPolicy::DEFAULT_HORIZON_PERIODS` (13) keeps a year and the
//!   reconciliation month, and `compact` refuses a horizon below
//!   `RetentionPolicy::MIN_HORIZON_PERIODS` (2) with `RetentionHorizonTooShort` rather than
//!   purging inside a producer's retry window. See `spot_metering::retention` for the invariant in
//!   full.
//! - **Replay beyond the horizon is a producer error, not a store bug.** A refused event is
//!   dropped, never counted; the producer still holds the fact and must either accept the loss
//!   (the horizon is a documented guarantee) or have its retry window shortened. Raising the
//!   horizon later does not help: the floor never recedes.
//! - Every value is a bound parameter; amounts are `numeric(20,0)` (a `u64` fits exactly); the
//!   store reads no clock; no error text carries an event id, a digest or a value.
//!
//! # Repair
//!
//! There is no rebuild-from-events command. After `compact` the event rows no longer hold the
//! amounts, and past the horizon they no longer hold the ids either, so a rebuild could only be
//! right for periods that were never compacted; a partial rebuild is a wrong total waiting to
//! happen. The totals are authoritative; the check that they match reality is
//! `spot_metering::reconcile` against an independent truth source, which this crate's tests run.
//!
//! # Errors
//!
//! A database or pool failure is reported as
//! `MeteringError::StoreUnavailable` with a stable short cause code and no SQLSTATE, host or
//! driver text (see [`error`]); `InvalidPeriodSpec` only ever means an invalid period spec.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, as the other `*-pg` crates do; CI sets it and the suite runs.

#![warn(missing_docs)]

pub mod error;
mod keys;
pub mod schema;
mod sql;
mod store;

pub use error::MIGRATION_FAILED;
pub use store::PgMeterStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-metering-pg";
