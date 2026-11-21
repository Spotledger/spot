//! The durable undo store of the remedy host: one row per undo token, so an operator can still
//! reverse a change after a restart.
//!
//! The framework `Registry` keeps its undo log in memory. The host copies every record it
//! stores ([`spot_remedies::UndoRecord`]) into an [`UndoStore`] the moment an apply succeeds,
//! restores the open ones into the live registry at start, and sweeps the expired ones.
//!
//! Two stores share the trait: [`PgUndoStore`] (the table `spot_remedy_undo` of the site's own
//! database) and [`MemoryUndoStore`] (a site with no database, and tests). Every error is a fixed
//! sentence ([`StoreError`]): it never carries a row, a token, a driver message or a value.
//!
//! What a row holds is exactly the serde form of the framework record (token, remedy, plan hash,
//! instants, the typed changes with their before and after fingerprints, the undo kind) plus the
//! site and the agent that applied it: no cell value of a business table beyond what the
//! framework's `Change` already holds.

use std::collections::BTreeMap;
use std::sync::Mutex;

use deadpool_postgres::Pool;
use spot_remedies::{UndoRecord, UndoToken};

use super::host::Fut;

const DDL: &str = "
CREATE TABLE IF NOT EXISTS spot_remedy_undo (
    token      text   PRIMARY KEY,
    site       text   NOT NULL,
    remedy     text   NOT NULL,
    agent      text   NOT NULL,
    applied_at bigint NOT NULL,
    expires_at bigint NOT NULL,
    record     jsonb  NOT NULL
);
CREATE INDEX IF NOT EXISTS spot_remedy_undo_expires ON spot_remedy_undo (expires_at);
";

/// What the store could not do. The text is fixed; it never quotes stored data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreError(pub &'static str);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for StoreError {}

fn pool_err<E>(_: E) -> StoreError {
    StoreError("the database is not available")
}

/// One undo record as the host hands it to the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredUndo {
    /// The site (tenant) whose host applied the change.
    pub site: String,
    /// The agent that applied it.
    pub agent: String,
    /// The framework's record.
    pub record: UndoRecord,
}

/// One row read back. A row whose JSON does not parse as an [`UndoRecord`] is still listed (with
/// `record` empty) so the host can keep it and report it instead of silently dropping it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEntry {
    /// The token column.
    pub token: String,
    /// The site column.
    pub site: String,
    /// The agent column.
    pub agent: String,
    /// The expiry column, unix seconds.
    pub expires_at: i64,
    /// The parsed record, or `None` when the stored JSON is not a valid record.
    pub record: Option<UndoRecord>,
}

/// Where undo records are kept.
pub trait UndoStore: Send + Sync {
    /// Creates what the store needs (idempotent).
    fn ensure_schema(&self) -> Fut<'_, Result<(), StoreError>>;

    /// Writes (or replaces) the row for the record's token.
    fn put<'a>(&'a self, row: &'a StoredUndo) -> Fut<'a, Result<(), StoreError>>;

    /// Deletes the row of a token (a missing row is not an error).
    fn remove<'a>(&'a self, token: &'a UndoToken) -> Fut<'a, Result<(), StoreError>>;

    /// Every row whose window has not closed at `now` (`expires_at >= now`), ordered by token.
    fn list_open(&self, now: i64) -> Fut<'_, Result<Vec<StoredEntry>, StoreError>>;

    /// Deletes every row whose window closed before `now` and returns how many went.
    fn sweep(&self, now: i64) -> Fut<'_, Result<u64, StoreError>>;
}

/// The table `spot_remedy_undo` of the site's own database.
#[derive(Clone)]
pub struct PgUndoStore {
    pool: Pool,
}

impl PgUndoStore {
    /// A store over the site's pool.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

impl UndoStore for PgUndoStore {
    fn ensure_schema(&self) -> Fut<'_, Result<(), StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(pool_err)?;
            client
                .batch_execute(DDL)
                .await
                .map_err(|_| StoreError("the undo table could not be created"))
        })
    }

    fn put<'a>(&'a self, row: &'a StoredUndo) -> Fut<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let text = serde_json::to_string(&row.record)
                .map_err(|_| StoreError("the undo record could not be encoded"))?;
            let client = self.pool.get().await.map_err(pool_err)?;
            let token = row.record.token.as_str().to_owned();
            let remedy = row.record.remedy.as_str().to_owned();
            client
                .execute(
                    "INSERT INTO spot_remedy_undo \
                     (token, site, remedy, agent, applied_at, expires_at, record) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7::text::jsonb) \
                     ON CONFLICT (token) DO UPDATE SET site = $2, remedy = $3, agent = $4, \
                     applied_at = $5, expires_at = $6, record = $7::text::jsonb",
                    &[
                        &token,
                        &row.site,
                        &remedy,
                        &row.agent,
                        &row.record.applied_at,
                        &row.record.expires_at,
                        &text,
                    ],
                )
                .await
                .map(|_| ())
                .map_err(|_| StoreError("the undo record could not be written"))
        })
    }

    fn remove<'a>(&'a self, token: &'a UndoToken) -> Fut<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(pool_err)?;
            let token = token.as_str().to_owned();
            client
                .execute("DELETE FROM spot_remedy_undo WHERE token = $1", &[&token])
                .await
                .map(|_| ())
                .map_err(|_| StoreError("the undo record could not be removed"))
        })
    }

    fn list_open(&self, now: i64) -> Fut<'_, Result<Vec<StoredEntry>, StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(pool_err)?;
            let rows = client
                .query(
                    "SELECT token, site, agent, expires_at, record::text \
                     FROM spot_remedy_undo WHERE expires_at >= $1 ORDER BY token",
                    &[&now],
                )
                .await
                .map_err(|_| StoreError("the undo records could not be read"))?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let unreadable = |_| StoreError("an undo row could not be decoded");
                let text: String = row.try_get(4).map_err(unreadable)?;
                out.push(StoredEntry {
                    token: row.try_get(0).map_err(unreadable)?,
                    site: row.try_get(1).map_err(unreadable)?,
                    agent: row.try_get(2).map_err(unreadable)?,
                    expires_at: row.try_get(3).map_err(unreadable)?,
                    record: serde_json::from_str(&text).ok(),
                });
            }
            Ok(out)
        })
    }

    fn sweep(&self, now: i64) -> Fut<'_, Result<u64, StoreError>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(pool_err)?;
            client
                .execute(
                    "DELETE FROM spot_remedy_undo WHERE expires_at < $1",
                    &[&now],
                )
                .await
                .map_err(|_| StoreError("the expired undo records could not be swept"))
        })
    }
}

/// One in-memory row: the same columns as the table, the record as JSON text (so a test can
/// plant a corrupt one and the key set of what is stored can be asserted).
#[derive(Clone, Debug)]
struct MemRow {
    site: String,
    agent: String,
    expires_at: i64,
    record: String,
}

/// An in-memory store: the fallback for a site with no database, and for tests. Nothing in it
/// survives the process, but several hosts may share one (an `Arc`), which is how a test
/// simulates a restart.
#[derive(Debug, Default)]
pub struct MemoryUndoStore {
    rows: Mutex<BTreeMap<String, MemRow>>,
}

impl MemoryUndoStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn rows(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, MemRow>> {
        self.rows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How many rows are held, expired or not.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows().len()
    }

    /// Whether the store holds no row.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows().is_empty()
    }

    /// The stored JSON text of a token's record, as it would sit in the `record` column.
    #[must_use]
    pub fn raw_record(&self, token: &UndoToken) -> Option<String> {
        self.rows()
            .get(token.as_str())
            .map(|row| row.record.clone())
    }

    /// Plants a row as is (a corrupt record, or a row of another site), for tests.
    pub fn plant_raw(&self, token: &str, site: &str, agent: &str, expires_at: i64, record: &str) {
        self.rows().insert(
            token.to_owned(),
            MemRow {
                site: site.to_owned(),
                agent: agent.to_owned(),
                expires_at,
                record: record.to_owned(),
            },
        );
    }
}

impl UndoStore for MemoryUndoStore {
    fn ensure_schema(&self) -> Fut<'_, Result<(), StoreError>> {
        Box::pin(async { Ok(()) })
    }

    fn put<'a>(&'a self, row: &'a StoredUndo) -> Fut<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let text = serde_json::to_string(&row.record)
                .map_err(|_| StoreError("the undo record could not be encoded"))?;
            self.rows().insert(
                row.record.token.as_str().to_owned(),
                MemRow {
                    site: row.site.clone(),
                    agent: row.agent.clone(),
                    expires_at: row.record.expires_at,
                    record: text,
                },
            );
            Ok(())
        })
    }

    fn remove<'a>(&'a self, token: &'a UndoToken) -> Fut<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.rows().remove(token.as_str());
            Ok(())
        })
    }

    fn list_open(&self, now: i64) -> Fut<'_, Result<Vec<StoredEntry>, StoreError>> {
        Box::pin(async move {
            Ok(self
                .rows()
                .iter()
                .filter(|(_, row)| row.expires_at >= now)
                .map(|(token, row)| StoredEntry {
                    token: token.clone(),
                    site: row.site.clone(),
                    agent: row.agent.clone(),
                    expires_at: row.expires_at,
                    record: serde_json::from_str(&row.record).ok(),
                })
                .collect())
        })
    }

    fn sweep(&self, now: i64) -> Fut<'_, Result<u64, StoreError>> {
        Box::pin(async move {
            let mut rows = self.rows();
            let before = rows.len();
            rows.retain(|_, row| row.expires_at >= now);
            Ok(u64::try_from(before - rows.len()).unwrap_or(u64::MAX))
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_remedies::{Change, PlanHash, RemedyId, UndoKind};

    fn record(token_seq: u64, expires_at: i64) -> UndoRecord {
        let remedy = RemedyId::new("job.retry_failed");
        let plan_hash = PlanHash::compute(&remedy, &serde_json::json!({}), &[]);
        UndoRecord {
            token: UndoToken::derive(&plan_hash, 100, token_seq),
            remedy,
            plan_hash,
            applied_at: 100,
            expires_at,
            changes: vec![Change {
                target: "job:j1".to_owned(),
                before: "failed".to_owned(),
                after: "queued".to_owned(),
            }],
            undo: UndoKind::reversible("restores the job"),
        }
    }

    fn row(record: UndoRecord) -> StoredUndo {
        StoredUndo {
            site: "site-1".to_owned(),
            agent: "agent-1".to_owned(),
            record,
        }
    }

    #[tokio::test]
    async fn put_list_remove_and_sweep_round_trip() {
        let store = MemoryUndoStore::new();
        store.ensure_schema().await.unwrap();
        let open = record(1, 500);
        let closed = record(2, 150);
        store.put(&row(open.clone())).await.unwrap();
        store.put(&row(closed.clone())).await.unwrap();
        assert_eq!(store.len(), 2);

        let listed = store.list_open(200).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].record.as_ref(), Some(&open));
        assert_eq!(listed[0].site, "site-1");

        assert_eq!(store.sweep(200).await.unwrap(), 1);
        assert_eq!(store.len(), 1);
        store.remove(&open.token).await.unwrap();
        assert!(store.is_empty());
        // Removing a missing row is not an error.
        store.remove(&open.token).await.unwrap();
    }

    #[tokio::test]
    async fn a_corrupt_row_is_listed_without_a_record() {
        let store = MemoryUndoStore::new();
        store.plant_raw("undo-bad", "site-1", "agent-1", 500, "{not json");
        let listed = store.list_open(0).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].record.is_none());
        assert_eq!(listed[0].token, "undo-bad");
    }

    #[test]
    fn a_store_error_is_its_fixed_sentence() {
        assert_eq!(
            StoreError("the database is not available").to_string(),
            "the database is not available"
        );
    }
}
