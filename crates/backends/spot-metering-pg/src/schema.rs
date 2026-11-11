//! The DDL of the metering tables.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_meter_event` | one row per `(tenant, event_id)`: the idempotency tombstone, plus the raw metadata (meter, amount, member digest, instant) until `compact` nulls it. `period_end` stays: it is the tombstone's retention key, not raw metadata |
//! | `spot_meter_period` | one row per tenant-period window (label, zone, basis, start, end), giving each window a surrogate id |
//! | `spot_meter_total` | one row per `(period, meter)`: the maintained counter sum, gauge maximum and distinct count |
//! | `spot_meter_member` | one row per `(period, meter, digest)`: the distinct-set population |
//! | `spot_meter_retention` | one row per tenant: the late-arrival floor, the first period still accepting events, set by compaction and never moved back. The row is created on the tenant's first write with a no-floor sentinel, so the write path always has a row to lock |
//!
//! Every row is reachable only through a tenant: the event table by its key, the others by
//! `spot_meter_period.tenant` or by their own tenant column. Indexes (ADR-045): the event primary
//! key serves the idempotent insert and the tombstone count; `spot_meter_event_compact_idx`
//! (partial, on rows that still hold raw metadata) serves the bounded compaction scan;
//! `spot_meter_event_tenant_idx (tenant, period_end)` serves the bounded tombstone purge; the
//! period unique key serves every totals lookup; the total and member primary keys start with the
//! period id.
//!
//! `period_end` is written on every insert and is no longer cleared by compaction: a tombstone
//! has to know which period it belongs to, or it could never be purged and the horizon would
//! never bite. Rows written by an older schema, whose period end was cleared, are not purged
//! (there is nothing left to date them by) and are refused by nothing; they are a startup
//! migration's business, not this store's.
//!
//! [`prepare`] is idempotent and serialised across nodes by a transaction-scoped advisory lock.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;

use crate::error::{migration, migration_pool};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x4D54_5247_0001;

/// The name of the constraint that bounds a counter sum to `u64`; a violation is an overflow.
pub const SUM_RANGE_CONSTRAINT: &str = "spot_meter_total_sum_range";

/// The name of the constraint that keeps the raw-metadata columns together.
pub const RAW_TOGETHER_CONSTRAINT: &str = "spot_meter_event_raw_metadata_together";

/// The idempotent DDL. Every statement is `IF NOT EXISTS` or a guarded drop-and-add.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_meter_event (
    tenant        text NOT NULL,
    event_id      text NOT NULL,
    meter         text,
    amount        numeric(20, 0),
    member_digest text,
    occurred_at   bigint,
    period_end    bigint,
    PRIMARY KEY (tenant, event_id)
);

ALTER TABLE spot_meter_event DROP CONSTRAINT IF EXISTS spot_meter_event_raw_together;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'spot_meter_event_raw_metadata_together'
          AND conrelid = 'spot_meter_event'::regclass
    ) THEN
        ALTER TABLE spot_meter_event ADD CONSTRAINT spot_meter_event_raw_metadata_together
            CHECK ((meter IS NULL) = (amount IS NULL)
                   AND (meter IS NULL) = (occurred_at IS NULL));
    END IF;
END $$;

CREATE INDEX IF NOT EXISTS spot_meter_event_compact_idx
    ON spot_meter_event (period_end) WHERE meter IS NOT NULL;

CREATE INDEX IF NOT EXISTS spot_meter_event_tenant_idx
    ON spot_meter_event (tenant, period_end);

CREATE TABLE IF NOT EXISTS spot_meter_period (
    period_id     bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant        text NOT NULL,
    period_start  bigint NOT NULL,
    period_end    bigint NOT NULL,
    label         text NOT NULL,
    zone          text NOT NULL,
    basis         text NOT NULL,
    CONSTRAINT spot_meter_period_window UNIQUE (tenant, period_start, period_end, label, zone, basis)
);

CREATE TABLE IF NOT EXISTS spot_meter_total (
    period_id      bigint NOT NULL REFERENCES spot_meter_period (period_id),
    meter          text NOT NULL,
    sum_value      numeric(20, 0) NOT NULL DEFAULT 0,
    max_value      numeric(20, 0) NOT NULL DEFAULT 0,
    distinct_count bigint NOT NULL DEFAULT 0 CHECK (distinct_count >= 0),
    PRIMARY KEY (period_id, meter),
    CONSTRAINT spot_meter_total_sum_range CHECK (sum_value >= 0 AND sum_value <= 18446744073709551615),
    CONSTRAINT spot_meter_total_max_range CHECK (max_value >= 0 AND max_value <= 18446744073709551615)
);

CREATE TABLE IF NOT EXISTS spot_meter_member (
    period_id bigint NOT NULL REFERENCES spot_meter_period (period_id),
    meter     text NOT NULL,
    digest    text NOT NULL,
    PRIMARY KEY (period_id, meter, digest)
);

CREATE TABLE IF NOT EXISTS spot_meter_retention (
    tenant        text PRIMARY KEY,
    period_start  bigint NOT NULL,
    period_end    bigint NOT NULL,
    label         text NOT NULL,
    zone          text NOT NULL,
    basis         text NOT NULL
);
"#;

/// Creates the tables and indexes if they are missing. Idempotent and safe to run from several
/// nodes at once.
///
/// # Errors
/// `metering_pg.migration_failed` when the database cannot apply the DDL.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(migration_pool)?;
    let tx = conn.transaction().await.map_err(migration)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(migration)?;
    tx.batch_execute(DDL).await.map_err(migration)?;
    tx.commit().await.map_err(migration)
}
