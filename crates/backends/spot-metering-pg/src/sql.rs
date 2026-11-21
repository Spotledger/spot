//! Every statement of the store, as constants. All values are bound parameters; nothing here is
//! built from caller input.
//!
//! Every hot filter is indexed (ADR-045): the event primary key `(tenant, event_id)` serves the
//! idempotent insert and the tombstone count, `spot_meter_event_compact_idx` (partial, on rows
//! that still hold raw metadata) serves the bounded compaction scan,
//! `spot_meter_event_tenant_idx (tenant, period_end)` serves the tombstone purge, the period
//! unique key serves every totals lookup, and the total and member primary keys start with the
//! period id. Every statement that names a tenant binds it, and the tenant is part of the key or
//! filter of every one of them (ADR-061).

/// Insert a batch of events, first write wins, and return the keys actually inserted.
/// `$1` tenants, `$2` event ids, `$3` meter keys, `$4` amounts (text, cast to numeric), `$5`
/// member digests (nullable), `$6` instants, `$7` period ends. Rows go in key order so two
/// batches overlapping in ids take their row locks in the same order.
pub const INSERT_EVENTS: &str = "\
INSERT INTO spot_meter_event (tenant, event_id, meter, amount, member_digest, occurred_at, period_end) \
SELECT tenant, event_id, meter, amount::numeric, member_digest, occurred_at, period_end \
FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::int8[], $7::int8[]) \
    AS e (tenant, event_id, meter, amount, member_digest, occurred_at, period_end) \
ORDER BY tenant, event_id \
ON CONFLICT (tenant, event_id) DO NOTHING \
RETURNING tenant, event_id";

/// Create a period window if missing. `$1` tenant, `$2` start, `$3` end, `$4` label, `$5` zone,
/// `$6` basis.
pub const INSERT_PERIOD: &str = "\
INSERT INTO spot_meter_period (tenant, period_start, period_end, label, zone, basis) \
VALUES ($1, $2, $3, $4, $5, $6) \
ON CONFLICT ON CONSTRAINT spot_meter_period_window DO NOTHING";

/// The id of a period window. Same parameters as [`INSERT_PERIOD`].
pub const SELECT_PERIOD: &str = "\
SELECT period_id FROM spot_meter_period \
WHERE tenant = $1 AND period_start = $2 AND period_end = $3 AND label = $4 AND zone = $5 AND basis = $6";

/// Add counter sums. `$1` period ids, `$2` meter keys, `$3` sums (text, cast to numeric).
pub const ADD_COUNTERS: &str = "\
INSERT INTO spot_meter_total (period_id, meter, sum_value) \
SELECT period_id, meter, amount::numeric \
FROM unnest($1::int8[], $2::text[], $3::text[]) AS c (period_id, meter, amount) \
ORDER BY period_id, meter \
ON CONFLICT (period_id, meter) DO UPDATE SET sum_value = spot_meter_total.sum_value + EXCLUDED.sum_value";

/// Raise gauge maxima. Same parameters as [`ADD_COUNTERS`].
pub const RAISE_GAUGES: &str = "\
INSERT INTO spot_meter_total (period_id, meter, max_value) \
SELECT period_id, meter, amount::numeric \
FROM unnest($1::int8[], $2::text[], $3::text[]) AS g (period_id, meter, amount) \
ORDER BY period_id, meter \
ON CONFLICT (period_id, meter) DO UPDATE SET max_value = GREATEST(spot_meter_total.max_value, EXCLUDED.max_value)";

/// Insert distinct members, returning the ones actually new. `$1` period ids, `$2` meter keys,
/// `$3` digests.
pub const INSERT_MEMBERS: &str = "\
INSERT INTO spot_meter_member (period_id, meter, digest) \
SELECT period_id, meter, digest \
FROM unnest($1::int8[], $2::text[], $3::text[]) AS m (period_id, meter, digest) \
ORDER BY period_id, meter, digest \
ON CONFLICT (period_id, meter, digest) DO NOTHING \
RETURNING period_id, meter";

/// Add to distinct counts (creating the total row). `$1` period ids, `$2` meter keys, `$3` the
/// number of newly inserted members.
pub const ADD_DISTINCT: &str = "\
INSERT INTO spot_meter_total (period_id, meter, distinct_count) \
SELECT period_id, meter, added \
FROM unnest($1::int8[], $2::text[], $3::int8[]) AS d (period_id, meter, added) \
ORDER BY period_id, meter \
ON CONFLICT (period_id, meter) DO UPDATE SET distinct_count = spot_meter_total.distinct_count + EXCLUDED.distinct_count";

/// The totals of one period. `$1` period id.
pub const SELECT_TOTALS: &str = "\
SELECT meter, sum_value::text, max_value::text, distinct_count \
FROM spot_meter_total WHERE period_id = $1 ORDER BY meter";

/// Every member of one period, sorted. `$1` period id.
pub const SELECT_MEMBERS: &str = "\
SELECT meter, digest FROM spot_meter_member WHERE period_id = $1 ORDER BY meter, digest";

/// The members of one meter in one period, sorted. `$1` period id, `$2` meter key.
pub const SELECT_METER_MEMBERS: &str = "\
SELECT digest FROM spot_meter_member WHERE period_id = $1 AND meter = $2 ORDER BY digest";

/// How many deduplication tombstones one tenant holds. `$1` tenant.
pub const COUNT_TOMBSTONES: &str = "\
SELECT count(*) FROM spot_meter_event WHERE tenant = $1";

/// The sentinel `period_start` of a retention row that names **no** floor: below every
/// representable period, so it refuses nothing and is reported as `None`.
///
/// A tenant's retention row is created with this value by [`ENSURE_FLOOR_ROW`] before the write
/// path reads it, because `FOR SHARE` locks nothing on a row that does not exist yet (a recorded security finding
/// review: the first-compaction double count).
pub const NO_FLOOR_START: i64 = i64::MIN;

/// The sentinel `period_end` that goes with [`NO_FLOOR_START`].
pub const NO_FLOOR_END_EXCLUSIVE: i64 = i64::MIN + 1;

/// Create the tenant's retention row with the no-floor sentinel if it does not exist yet.
/// `$1` tenant, `$2` [`NO_FLOOR_START`], `$3` [`NO_FLOOR_END_EXCLUSIVE`].
///
/// This runs first in the write transaction, so [`SELECT_FLOOR_FOR_ADD`]'s `FOR SHARE` has a real
/// row to lock from the tenant's very first batch. Without it a batch could read "no floor", a
/// concurrent first compaction could advance the floor and purge the tombstone of an old id, and
/// the batch would then insert that replayed id as new — a double count. With the sentinel
/// inserted in the same transaction, either the batch's row lock is taken first and
/// [`ADVANCE_FLOOR`] waits for it, or the advance is committed first and the batch reads the new
/// floor and refuses the period.
pub const ENSURE_FLOOR_ROW: &str = "\
INSERT INTO spot_meter_retention (tenant, period_start, period_end, label, zone, basis) \
VALUES ($1, $2, $3, '', '', 'calendar') \
ON CONFLICT (tenant) DO NOTHING";

/// The tenant's late-arrival floor, if a compaction has ever set one. `$1` tenant.
///
/// Read `FOR SHARE` on the write path, **after** [`ENSURE_FLOOR_ROW`] has guaranteed the row
/// exists: a batch holding this lock cannot write an event into a period a concurrent compaction
/// is about to purge, so the refuse-or-count decision is made against one floor. A row that still
/// carries the [`NO_FLOOR_START`] sentinel reads as `None` and refuses nothing.
pub const SELECT_FLOOR_FOR_ADD: &str = "\
SELECT period_start, period_end, label, zone, basis FROM spot_meter_retention \
WHERE tenant = $1 FOR SHARE";

/// The tenant's late-arrival floor, outside a write transaction. `$1` tenant.
pub const SELECT_FLOOR: &str = "\
SELECT period_start, period_end, label, zone, basis FROM spot_meter_retention WHERE tenant = $1";

/// Advance the tenant's late-arrival floor, never backwards. `$1` tenant, `$2` start, `$3` end,
/// `$4` label, `$5` zone, `$6` basis; the row is written only when the new cut is later than the
/// recorded one (a [`NO_FLOOR_START`] sentinel is always earlier, so the first real cut lands).
///
/// This runs **before** [`DELETE_TOMBSTONES`], and its `ON CONFLICT DO UPDATE` waits for any
/// writer that holds the shared floor lock, so a batch that read the old floor finishes its
/// inserts before the purge can delete the tombstones it is using. A crash between the two leaves
/// tombstones that the next run removes, which wastes a row but can never re-count a purged id.
pub const ADVANCE_FLOOR: &str = "\
INSERT INTO spot_meter_retention (tenant, period_start, period_end, label, zone, basis) \
VALUES ($1, $2, $3, $4, $5, $6) \
ON CONFLICT (tenant) DO UPDATE \
SET period_start = EXCLUDED.period_start, period_end = EXCLUDED.period_end, \
    label = EXCLUDED.label, zone = EXCLUDED.zone, basis = EXCLUDED.basis \
WHERE spot_meter_retention.period_start < EXCLUDED.period_start";

/// Delete one bounded batch of tombstones of periods at or before the floor. `$1` tenant,
/// `$2` floor start, `$3` batch size. Returns the rows deleted.
///
/// The same predicate the write path refuses on (`period_end <= floor start`), so the ids removed
/// here are exactly the ids no future event may use.
pub const DELETE_TOMBSTONES: &str = "\
DELETE FROM spot_meter_event \
WHERE (tenant, event_id) IN ( \
    SELECT tenant, event_id FROM spot_meter_event \
    WHERE tenant = $1 AND period_end IS NOT NULL AND period_end <= $2 \
    ORDER BY period_end, event_id \
    LIMIT $3 \
)";

/// Null the raw metadata of one bounded batch of events of periods ending at or before the
/// cut, keeping the id tombstones and the period end they are retained by. `$1` tenant, `$2` cut
/// (period start of the first kept period), `$3` batch size. Returns the rows changed.
pub const COMPACT_BATCH: &str = "\
UPDATE spot_meter_event e \
SET meter = NULL, amount = NULL, member_digest = NULL, occurred_at = NULL \
FROM ( \
    SELECT tenant, event_id FROM spot_meter_event \
    WHERE tenant = $1 AND meter IS NOT NULL AND period_end <= $2 \
    ORDER BY period_end, event_id \
    LIMIT $3 \
    FOR UPDATE SKIP LOCKED \
) s \
WHERE e.tenant = s.tenant AND e.event_id = s.event_id";
