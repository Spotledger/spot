//! Outbox position adoption for a rebuilt database (WP-RTB-01.T6, ADR-061 section 6).
//!
//! A database rebuilt from the journal holds the journal's records at the journal's positions, so
//! **positions are preserved**: its outbox head is set to the journal position the rebuild ended
//! at, the next published entry is `(generation, seq + 1)` and every consumer bookmark taken
//! against the original stays valid.
//!
//! Both functions are **administrative**: they are not part of the storage contract and are used
//! only by the rebuild host, inside its own offline run. They take both logs' advisory locks, in
//! the publisher's order, so no publisher runs while a head moves, and they are never reachable
//! from a request.
//!
//! * [`adopt_position`]: legal only when the feed is **empty or its head is at or below** the
//!   target `(generation, seq)`. A head past the target is `feed_adopt.would_regress` (a feed
//!   never moves backwards). A newer target generation than the feed's starts that generation and
//!   resets the change log, as a restore does ([`super::start_new_generation`]).
//! * [`open_generation`]: opens generation `generation` (greater than the feed's) at `seq = 0`, so
//!   the next published entry is `(generation, 1)`; used for a point-in-time fork. The abandoned
//!   generation's published entries are removed from this database (the journal keeps them).

use spot_storage_contract::{ErrorKind, StorageError, StorageResult};
use tokio_postgres::IsolationLevel;

use super::publish::Log;
use super::{db, sql};
use crate::PgBackend;
use crate::schema::names::{META_TABLE, quote};

/// The refusal text of a head that would move backwards.
pub const WOULD_REGRESS: &str =
    "feed_adopt.would_regress: the feed head is past the position to adopt";
/// The refusal text of a generation that does not move forward.
pub const GENERATION_NOT_NEWER: &str =
    "feed_adopt.generation_not_newer: the generation to open is not newer than the feed's";

fn refuse(text: &str) -> StorageError {
    StorageError::new(ErrorKind::InvalidValue, text)
}

fn as_i64(value: u64, what: &str) -> StorageResult<i64> {
    i64::try_from(value)
        .map_err(|_| StorageError::new(ErrorKind::InvalidValue, format!("{what} is out of range")))
}

/// Sets the outbox head to `(generation, seq)`; see the module docs.
///
/// # Errors
/// `feed_adopt.would_regress` ([`ErrorKind::InvalidValue`]) when the feed's head is past the
/// position, otherwise the database's own error.
pub async fn adopt_position(backend: &PgBackend, generation: u64, seq: u64) -> StorageResult<()> {
    let target_generation = as_i64(generation, "the generation")?;
    let target_seq = as_i64(seq, "the sequence number")?;
    let mut conn = backend.checkout().await?;
    let tx = conn
        .build_transaction()
        .isolation_level(IsolationLevel::ReadCommitted)
        .start()
        .await
        .map_err(db)?;
    for log in [Log::Outbox, Log::Changes] {
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&log.lock_key()])
            .await
            .map_err(db)?;
    }
    let row = tx
        .query_one(sql::STATE, &[&Log::Outbox.name()])
        .await
        .map_err(db)?;
    let current_generation: i64 = row.get(0);
    let head: i64 = row.get(1);
    if current_generation > target_generation
        || (current_generation == target_generation && head > target_seq)
    {
        return Err(refuse(WOULD_REGRESS));
    }
    if current_generation < target_generation {
        // A newer generation than the feed's: it starts there, both logs from zero.
        tx.execute(
            &format!(
                "UPDATE {} SET value = $2::bigint::text WHERE key = $1",
                quote(META_TABLE)
            ),
            &[&sql::META_GENERATION, &target_generation],
        )
        .await
        .map_err(db)?;
        tx.execute(sql::RESET_STATE, &[]).await.map_err(db)?;
        prune_published(&tx).await?;
    }
    tx.execute(
        "UPDATE spot_feed_state SET last_seq = $1, floor_seq = $1 WHERE log = 'outbox'",
        &[&target_seq],
    )
    .await
    .map_err(db)?;
    tx.commit().await.map_err(db)
}

/// Opens generation `generation` at `seq = 0` for a fork; see the module docs.
///
/// # Errors
/// `feed_adopt.generation_not_newer` ([`ErrorKind::InvalidValue`]) when `generation` is not
/// greater than the feed's, otherwise the database's own error.
pub async fn open_generation(backend: &PgBackend, generation: u64) -> StorageResult<()> {
    let target_generation = as_i64(generation, "the generation")?;
    let mut conn = backend.checkout().await?;
    let tx = conn
        .build_transaction()
        .isolation_level(IsolationLevel::ReadCommitted)
        .start()
        .await
        .map_err(db)?;
    for log in [Log::Outbox, Log::Changes] {
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&log.lock_key()])
            .await
            .map_err(db)?;
    }
    let current: i64 = tx
        .query_one(sql::STATE, &[&Log::Outbox.name()])
        .await
        .map_err(db)?
        .get(0);
    if target_generation <= current {
        return Err(refuse(GENERATION_NOT_NEWER));
    }
    tx.execute(
        &format!(
            "UPDATE {} SET value = $2::bigint::text WHERE key = $1",
            quote(META_TABLE)
        ),
        &[&sql::META_GENERATION, &target_generation],
    )
    .await
    .map_err(db)?;
    tx.execute(sql::RESET_STATE, &[]).await.map_err(db)?;
    prune_published(&tx).await?;
    tx.commit().await.map_err(db)
}

/// Removes what an earlier generation published, as [`super::start_new_generation`] does; what is
/// still pending stays and is published into the new generation.
async fn prune_published(tx: &tokio_postgres::Transaction<'_>) -> StorageResult<()> {
    tx.execute(
        "SELECT set_config('spot.feed_maintenance', 'on', true)",
        &[],
    )
    .await
    .map_err(db)?;
    let keep_all: Option<f64> = None;
    tx.query_one(sql::PRUNE_OUTBOX, &[&keep_all])
        .await
        .map_err(db)?;
    tx.query_one(sql::PRUNE_CHANGES, &[&keep_all])
        .await
        .map_err(db)?;
    Ok(())
}
