//! The run log: one record per attempt, readable as the `connector_run` DocType.
//!
//! A record holds identifiers, an outcome, a duration and a stable error code: never an input, an
//! output, a header or any text from the upstream, so a hostile endpoint cannot put content in it.

use std::sync::Mutex;

use super::defs::{Boxed, STORE_FAILED};

/// How an attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It succeeded.
    Ok,
    /// It failed and will be retried.
    Retry,
    /// It failed for good (a permanent error).
    Failed,
    /// It failed on its last attempt; the connector waits for an operator.
    DeadLetter,
}

impl Outcome {
    /// The bounded label and stored text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Retry => "retry",
            Outcome::Failed => "failed",
            Outcome::DeadLetter => "dead_letter",
        }
    }
}

/// One attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRecord {
    /// The site.
    pub site: String,
    /// The connector.
    pub connector: String,
    /// The operation.
    pub operation: String,
    /// How it ended.
    pub outcome: Outcome,
    /// How long it took.
    pub duration_ms: u64,
    /// The stable error code, when it failed.
    pub code: Option<String>,
    /// The attempt number, from 1.
    pub attempt: u32,
    /// Microseconds since the Unix epoch, from the site clock.
    pub at_micros: i64,
}

/// Appends and reads run records.
pub trait RunLog: Send + Sync {
    /// Appends one record.
    fn append<'a>(&'a self, record: RunRecord) -> Boxed<'a, Result<(), String>>;
    /// The latest `limit` records of `connector`, newest first.
    fn latest<'a>(
        &'a self,
        site: &'a str,
        connector: &'a str,
        limit: u32,
    ) -> Boxed<'a, Result<Vec<RunRecord>, String>>;
}

/// An in-memory log.
#[derive(Default)]
pub struct MemoryRunLog {
    items: Mutex<Vec<RunRecord>>,
}

impl MemoryRunLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every record, oldest first.
    #[must_use]
    pub fn all(&self) -> Vec<RunRecord> {
        self.items.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl RunLog for MemoryRunLog {
    fn append<'a>(&'a self, record: RunRecord) -> Boxed<'a, Result<(), String>> {
        Box::pin(async move {
            self.items
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(record);
            Ok(())
        })
    }

    fn latest<'a>(
        &'a self,
        site: &'a str,
        connector: &'a str,
        limit: u32,
    ) -> Boxed<'a, Result<Vec<RunRecord>, String>> {
        Box::pin(async move {
            let items = self.items.lock().unwrap_or_else(|p| p.into_inner());
            Ok(items
                .iter()
                .rev()
                .filter(|r| r.site == site && r.connector == connector)
                .take(limit as usize)
                .cloned()
                .collect())
        })
    }
}

/// The PostgreSQL run log, table `spot_connector_run`.
pub struct PgRunLog {
    pool: deadpool_postgres::Pool,
}

impl PgRunLog {
    /// A log over `pool`.
    #[must_use]
    pub fn new(pool: deadpool_postgres::Pool) -> Self {
        Self { pool }
    }

    /// Creates the table if it is missing.
    ///
    /// # Errors
    ///
    /// `connector.store_failed`.
    pub async fn prepare(&self) -> Result<(), String> {
        let client = self.pool.get().await.map_err(|_| STORE_FAILED.to_owned())?;
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS spot_connector_run (\
                 id bigserial PRIMARY KEY, site text NOT NULL, connector text NOT NULL, \
                 operation text NOT NULL, outcome text NOT NULL, duration_ms bigint NOT NULL, \
                 code text, attempt integer NOT NULL, at_micros bigint NOT NULL);\
                 CREATE INDEX IF NOT EXISTS spot_connector_run_idx \
                 ON spot_connector_run (site, connector, id DESC)",
            )
            .await
            .map_err(|_| STORE_FAILED.to_owned())
    }
}

impl RunLog for PgRunLog {
    fn append<'a>(&'a self, record: RunRecord) -> Boxed<'a, Result<(), String>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| STORE_FAILED.to_owned())?;
            let duration = i64::try_from(record.duration_ms).unwrap_or(i64::MAX);
            let attempt = i32::try_from(record.attempt).unwrap_or(i32::MAX);
            client
                .execute(
                    "INSERT INTO spot_connector_run \
                     (site, connector, operation, outcome, duration_ms, code, attempt, at_micros) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                    &[
                        &record.site,
                        &record.connector,
                        &record.operation,
                        &record.outcome.as_str(),
                        &duration,
                        &record.code,
                        &attempt,
                        &record.at_micros,
                    ],
                )
                .await
                .map(|_| ())
                .map_err(|_| STORE_FAILED.to_owned())
        })
    }

    fn latest<'a>(
        &'a self,
        site: &'a str,
        connector: &'a str,
        limit: u32,
    ) -> Boxed<'a, Result<Vec<RunRecord>, String>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| STORE_FAILED.to_owned())?;
            let rows = client
                .query(
                    "SELECT operation, outcome, duration_ms, code, attempt, at_micros \
                     FROM spot_connector_run WHERE site = $1 AND connector = $2 \
                     ORDER BY id DESC LIMIT $3",
                    &[&site, &connector, &i64::from(limit)],
                )
                .await
                .map_err(|_| STORE_FAILED.to_owned())?;
            Ok(rows
                .iter()
                .map(|row| {
                    let outcome: String = row.get(1);
                    let duration: i64 = row.get(2);
                    let attempt: i32 = row.get(4);
                    RunRecord {
                        site: site.to_owned(),
                        connector: connector.to_owned(),
                        operation: row.get(0),
                        outcome: match outcome.as_str() {
                            "ok" => Outcome::Ok,
                            "retry" => Outcome::Retry,
                            "dead_letter" => Outcome::DeadLetter,
                            _ => Outcome::Failed,
                        },
                        duration_ms: u64::try_from(duration).unwrap_or(0),
                        code: row.get(3),
                        attempt: u32::try_from(attempt).unwrap_or(0),
                        at_micros: row.get(5),
                    }
                })
                .collect())
        })
    }
}
