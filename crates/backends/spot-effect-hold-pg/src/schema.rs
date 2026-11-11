//! The DDL of the side-effect hold ledger, and its indexes.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_effect_hold_effect` | one row per held effect: the site, its place in the site's original order (`seq`), the idempotency key, the class and kind, the ordering source and outbox event, the creation instant, the payload reference, the migration that held it, its state, how many times a release took it into `releasing` (`attempts`) and an optional note |
//! | `spot_effect_hold_sequence` | the high-water mark of each site's sequence, so a number is allocated inside the insert transaction and never reused |
//! | `spot_effect_hold_state` | one row per site that was ever held: the [`HoldState`](spot_effect_hold::HoldState) as `jsonb`, compared and swapped whole |
//! | `spot_effect_hold_checkpoint` | a release's progress per `(site, migration)`, so a crash mid-release resumes instead of restarting |
//!
//! Indexes (ADR-045: every hot filter is covered):
//!
//! - `spot_effect_hold_effect_site_key_uniq`, unique on `(site, effect_key)`: deduplication is an
//!   index probe, and the database itself refuses a second row for one key.
//! - `spot_effect_hold_effect_site_seq_uniq`, unique on `(site, seq)`: the original order, and the
//!   proof that a sequence number is never handed out twice.
//! - `spot_effect_hold_effect_list_idx (site, status, seq)`: `list_effects` — one site, some states,
//!   in sequence order.
//!
//! Every value reaches SQL as a bound parameter (ADR-045); the only text in these statements is
//! this fixed DDL.
//!
//! # Two processes preparing at once
//!
//! `CREATE TABLE IF NOT EXISTS` is not by itself safe against a concurrent creator: the loser can
//! fail on a duplicate `pg_class` row. [`DDL`] therefore opens with a **transaction-scoped**
//! advisory lock, held until the transaction commits, so a second `prepare` waits instead of
//! racing. It is deliberately not a session lock, which a pooled connection would leak.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;

use crate::error::{migration, migration_pool};

/// The idempotent DDL, one transaction, serialised across nodes by its advisory lock.
pub const DDL: &str = "\
SELECT pg_advisory_xact_lock(hashtext('spot-effect-hold-pg:prepare')); \
CREATE TABLE IF NOT EXISTS spot_effect_hold_effect (\
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
    site text NOT NULL, \
    seq bigint NOT NULL CHECK (seq > 0), \
    effect_key text NOT NULL, \
    effect_class jsonb NOT NULL, \
    effect_class_key text NOT NULL, \
    kind text NOT NULL, \
    source text NOT NULL, \
    source_event text, \
    created_at bigint NOT NULL, \
    payload_ref jsonb NOT NULL, \
    migration_id text NOT NULL, \
    status text NOT NULL CHECK \
        (status IN ('held', 'releasing', 'dispatched', 'discarded', 'needs_confirmation')), \
    note text, \
    attempts bigint NOT NULL DEFAULT 0 CHECK (attempts >= 0), \
    CONSTRAINT spot_effect_hold_effect_site_key_uniq UNIQUE (site, effect_key), \
    CONSTRAINT spot_effect_hold_effect_site_seq_uniq UNIQUE (site, seq)); \
ALTER TABLE spot_effect_hold_effect \
    ADD COLUMN IF NOT EXISTS attempts bigint NOT NULL DEFAULT 0 CHECK (attempts >= 0); \
CREATE INDEX IF NOT EXISTS spot_effect_hold_effect_list_idx \
    ON spot_effect_hold_effect (site, status, seq); \
CREATE TABLE IF NOT EXISTS spot_effect_hold_sequence (\
    site text PRIMARY KEY, \
    last_sequence bigint NOT NULL CHECK (last_sequence > 0)); \
CREATE TABLE IF NOT EXISTS spot_effect_hold_state (\
    site text PRIMARY KEY, \
    state jsonb NOT NULL, \
    CONSTRAINT spot_effect_hold_state_shape CHECK ( \
        jsonb_typeof(state) = 'object' \
        AND state ? 'hold' \
        AND jsonb_typeof(state -> 'hold') IN ('string', 'object'))); \
CREATE TABLE IF NOT EXISTS spot_effect_hold_checkpoint (\
    site text NOT NULL, \
    migration_id text NOT NULL, \
    dispatched numeric(20, 0) NOT NULL CHECK (dispatched >= 0), \
    last_sequence numeric(20, 0) CHECK (last_sequence >= 0), \
    needs_confirmation numeric(20, 0) NOT NULL CHECK (needs_confirmation >= 0), \
    PRIMARY KEY (site, migration_id));";

/// Creates the tables and indexes if they are missing. Idempotent and safe to run from several
/// nodes at once.
///
/// # Errors
/// `effect_hold_pg.migration_failed` when the database cannot apply the DDL.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut client = pool.get().await.map_err(|e| migration_pool(&e))?;
    let tx = client.transaction().await.map_err(|e| migration(&e))?;
    tx.batch_execute(DDL).await.map_err(|e| migration(&e))?;
    tx.commit().await.map_err(|e| migration(&e))?;
    Ok(())
}
