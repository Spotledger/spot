//! Administration of the feed: the generation, the retention setting and the pruning (ADR-053).

use std::time::Duration;

use spot_storage_contract::{ErrorKind, StorageError, StorageResult};
use tokio_postgres::IsolationLevel;

use super::publish::Log;
use super::{db, sql};
use crate::PgBackend;
use crate::schema::names::{META_TABLE, quote};

/// What [`cleanup`] removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cleanup {
    /// Outbox events removed.
    pub outbox_events: u64,
    /// Change entries removed.
    pub changes: u64,
}

/// The retention in force: how long published entries are kept at least. Default 7 days.
pub async fn retention(backend: &PgBackend) -> StorageResult<Duration> {
    let conn = backend.checkout().await?;
    let row = conn
        .query_opt(
            &format!("SELECT value FROM {} WHERE key = $1", quote(META_TABLE)),
            &[&sql::META_RETENTION],
        )
        .await
        .map_err(db)?;
    let secs = match row {
        None => sql::DEFAULT_RETENTION_SECS,
        Some(r) => r.get::<_, String>(0).parse().map_err(|_| {
            StorageError::new(ErrorKind::Internal, "feed retention is not a number")
        })?,
    };
    Ok(Duration::from_secs(secs))
}

/// Sets the retention. Entries younger than this are never pruned by [`cleanup`]. A retention
/// of zero is refused: consumers need time to read.
pub async fn set_retention(backend: &PgBackend, retention: Duration) -> StorageResult<()> {
    if retention.is_zero() {
        return Err(StorageError::new(
            ErrorKind::InvalidValue,
            "the feed retention must be longer than zero",
        ));
    }
    let conn = backend.checkout().await?;
    let secs = retention.as_secs().to_string();
    conn.execute(
        &format!(
            "INSERT INTO {} (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            quote(META_TABLE)
        ),
        &[&sql::META_RETENTION, &secs],
    )
    .await
    .map_err(db)?;
    Ok(())
}

/// Prunes published entries older than the retention (the newest entry of each log is always
/// kept) and everything left from earlier generations. A job calls this regularly (WP-PLT-06).
/// Bookmarks below what remains then fail with `ResyncRequired`.
pub async fn cleanup(backend: &PgBackend) -> StorageResult<Cleanup> {
    let secs = retention(backend).await?.as_secs_f64();
    let mut out = Cleanup::default();
    for log in [Log::Outbox, Log::Changes] {
        let mut conn = backend.checkout().await?;
        let tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::ReadCommitted)
            .start()
            .await
            .map_err(db)?;
        // The publisher's lock: no publication runs while positions are removed.
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&log.lock_key()])
            .await
            .map_err(db)?;
        // Lets this transaction, and only it, delete from the insert-only outbox.
        tx.execute(
            "SELECT set_config('spot.feed_maintenance', 'on', true)",
            &[],
        )
        .await
        .map_err(db)?;
        let statement = match log {
            Log::Outbox => sql::PRUNE_OUTBOX,
            Log::Changes => sql::PRUNE_CHANGES,
        };
        let removed: i64 = tx.query_one(statement, &[&secs]).await.map_err(db)?.get(0);
        tx.commit().await.map_err(db)?;
        let removed = u64::try_from(removed).unwrap_or(0);
        match log {
            Log::Outbox => out.outbox_events = removed,
            Log::Changes => out.changes = removed,
        }
    }
    Ok(out)
}

/// Starts a new stream generation and returns it: the admin function a **restore from backup
/// calls** once the restored database is up (the restored feed no longer continues what
/// consumers read). Both logs begin again at sequence 0 in the new generation; every bookmark of
/// an older generation fails with `ResyncRequired`, and consumers rebuild from the start.
/// Entries already published are removed; entries still pending (committed but not yet
/// published) are published into the new generation.
pub async fn start_new_generation(backend: &PgBackend) -> StorageResult<u64> {
    let mut conn = backend.checkout().await?;
    let tx = conn
        .build_transaction()
        .isolation_level(IsolationLevel::ReadCommitted)
        .start()
        .await
        .map_err(db)?;
    // Both publishers' locks, always in the same order.
    for log in [Log::Outbox, Log::Changes] {
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&log.lock_key()])
            .await
            .map_err(db)?;
    }
    let next: i64 = tx
        .query_one(
            &format!(
                "UPDATE {} SET value = (value::bigint + 1)::text WHERE key = $1 RETURNING value::bigint",
                quote(META_TABLE)
            ),
            &[&sql::META_GENERATION],
        )
        .await
        .map_err(db)?
        .get(0);
    tx.execute(sql::RESET_STATE, &[]).await.map_err(db)?;
    // Whatever was published in an earlier generation goes; what is still pending stays and is
    // published into the new generation.
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
    tx.commit().await.map_err(db)?;
    u64::try_from(next)
        .map_err(|_| StorageError::new(ErrorKind::Internal, "negative feed generation"))
}
