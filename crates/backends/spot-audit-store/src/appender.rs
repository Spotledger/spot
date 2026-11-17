//! [`PgAppenderStore`]: the lane appender's durable state in PostgreSQL (WP-AI-11.T5, async since
//! WP-SEC-06.T6).
//!
//! `spot_doc_audit::LaneAppender` keeps four things in an [`AppenderStore`]: each lane's outbox
//! bookmark, the event-id index that makes a re-delivered event chain once, the per-document
//! index the review and the digest read, and the newest signed site checkpoint. The in-memory
//! reference forgets them on a restart, which re-chains every event; this store keeps them in
//! four `spot_audit_*` tables, every key starting with the site, so a store for site A can never
//! read or write site B's rows.
//!
//! # An async port over the pool
//!
//! The trait is async and fallible (ADR-082 section 1, class P by mechanism), and this store
//! implements it directly over a `deadpool_postgres::Pool`: each call checks a connection out,
//! runs one statement (or one statement and its read-back) and returns it. There is no blocking
//! bridge, so a job running inside the tokio runtime can drive it. A failure is returned as
//! `docaudit.appender_state` with the store's own code in the message; the appender ends the
//! drain at once, leaves the bookmark where it was and the job retries.
//!
//! # Deadline and retry
//!
//! Each pool checkout and each statement runs under [`tokio::time::timeout`] with the store's
//! deadline ([`DEFAULT_DEADLINE`], [`PgAppenderStore::with_deadline`]); a timeout is
//! `auditstore.unavailable`. A pool or connection failure, or a timeout, is tried again a few
//! times after a short pause. **Every statement is a read or an idempotent upsert**, so a repeat
//! after an unknown outcome is safe; a refusal by the database (a constraint, a missing table)
//! is not retried.
//!
//! # Idempotence
//!
//! `set_bookmark`, `record_appended` and `record_document_entry` are idempotent upserts. An event
//! id recorded again at the same `(lane, seq)` changes nothing; recorded at a different
//! `(lane, seq)` it is refused with [`EVENT_CONFLICT`] and the stored row is kept.
//!
//! # No value in an error
//!
//! Keys are the site, event id, doctype, document id and lane/seq. An error names the operation
//! and, for a database refusal, the SQLSTATE; it never carries an event id, a document id, a
//! checkpoint or any value, and the store logs nothing.

use std::time::Duration;

use deadpool_postgres::Pool;
use spot_doc_audit::{AppenderStore, DocAuditError, SiteCheckpoint, StateFuture};
use spot_storage_contract::feed::FeedPosition;
use tokio_postgres::Error as PgError;

use crate::appender_sql::{
    BOOKMARK_GET, BOOKMARK_SET, CHECKPOINT_GET, CHECKPOINT_SET, DOC_INSERT, DOC_INSERT_MANY,
    DOC_LIST, EVENT_GET, EVENT_GET_MANY, EVENT_INSERT, EVENT_INSERT_MANY,
};
use crate::error::{EVENT_CONFLICT, Error, Result, corrupt, db, pool};

/// The deadline of one database call unless [`PgAppenderStore::with_deadline`] sets another.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(5);

/// How many times a call is tried when the failure is a pool, connection or timeout failure.
const ATTEMPTS: u32 = 3;

/// The pause between two attempts.
const RETRY_PAUSE: Duration = Duration::from_millis(25);

/// The appender state of one site in PostgreSQL. See the module documentation.
pub struct PgAppenderStore {
    pool: Pool,
    site: String,
    deadline: Duration,
}

/// One attempt's failure: the error and whether trying again can help.
type Attempt<T> = std::result::Result<T, (bool, Error)>;

impl PgAppenderStore {
    /// A store for `site` over `pool`, with [`DEFAULT_DEADLINE`].
    ///
    /// Nothing is read or created: the tables come from the migrator
    /// ([`crate::APPENDER_TABLE_DDL`]) and a host checks them with [`crate::verify_appender`]
    /// before it serves the site.
    #[must_use]
    pub fn new(pool: Pool, site: impl Into<String>) -> Self {
        Self {
            pool,
            site: site.into(),
            deadline: DEFAULT_DEADLINE,
        }
    }

    /// The same store with `deadline` as the time budget of one pool checkout and of one
    /// statement.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// The site this store is scoped to.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }

    /// Checks a connection out under the deadline. A failure here is always worth a retry.
    async fn checkout(&self, what: &str) -> Attempt<deadpool_postgres::Object> {
        match tokio::time::timeout(self.deadline, self.pool.get()).await {
            Ok(Ok(client)) => Ok(client),
            Ok(Err(e)) => Err((true, pool(what, &e))),
            Err(_) => Err((
                true,
                Error::unavailable(format!("{what}: no connection within the deadline")),
            )),
        }
    }

    /// Runs one statement body under the deadline on a checked-out connection.
    async fn run_once<T, Fut>(&self, what: &str, body: Fut) -> Attempt<T>
    where
        Fut: std::future::Future<Output = std::result::Result<T, PgError>>,
    {
        match tokio::time::timeout(self.deadline, body).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err((retryable(&e), db(what, &e))),
            Err(_) => Err((
                true,
                Error::unavailable(format!("{what}: the statement timed out")),
            )),
        }
    }

    /// The pause and the verdict between two attempts; `Ok(())` means try again.
    async fn between(attempt: &mut u32, failure: (bool, Error)) -> Result<()> {
        let (again, error) = failure;
        if again && *attempt < ATTEMPTS {
            *attempt += 1;
            tokio::time::sleep(RETRY_PAUSE).await;
            Ok(())
        } else {
            Err(error)
        }
    }

    async fn bookmark_of(&self, lane: u32) -> Result<FeedPosition> {
        let what = "read bookmark";
        let lane = i64::from(lane);
        let mut attempt = 1;
        let row = loop {
            let outcome = async {
                let client = self.checkout(what).await?;
                self.run_once(what, async {
                    match client.query_opt(BOOKMARK_GET, &[&self.site, &lane]).await? {
                        Some(row) => {
                            Ok(Some((row.try_get::<_, i64>(0)?, row.try_get::<_, i64>(1)?)))
                        }
                        None => Ok(None),
                    }
                })
                .await
            }
            .await;
            match outcome {
                Ok(row) => break row,
                Err(failure) => Self::between(&mut attempt, failure).await?,
            }
        };
        match row {
            None => Ok(FeedPosition::START),
            Some((generation, seq)) => Ok(FeedPosition::new(
                unsigned(generation, "bookmark")?,
                unsigned(seq, "bookmark")?,
            )),
        }
    }

    async fn store_bookmark(&self, lane: u32, position: FeedPosition) -> Result<()> {
        let what = "store bookmark";
        let lane = i64::from(lane);
        let generation = signed(position.generation(), "bookmark")?;
        let seq = signed(position.seq(), "bookmark")?;
        let mut attempt = 1;
        loop {
            let outcome = async {
                let client = self.checkout(what).await?;
                self.run_once(what, async {
                    let statement = client.prepare_cached(BOOKMARK_SET).await?;
                    client
                        .execute(&statement, &[&self.site, &lane, &generation, &seq])
                        .await
                        .map(|_| ())
                })
                .await
            }
            .await;
            match outcome {
                Ok(()) => return Ok(()),
                Err(failure) => Self::between(&mut attempt, failure).await?,
            }
        }
    }

    async fn event_of(&self, event_id: &str) -> Result<Option<(u32, u64)>> {
        let what = "read event index";
        let mut attempt = 1;
        let row = loop {
            let outcome = async {
                let client = self.checkout(what).await?;
                self.run_once(what, async {
                    match client
                        .query_opt(EVENT_GET, &[&self.site, &event_id])
                        .await?
                    {
                        Some(row) => {
                            Ok(Some((row.try_get::<_, i64>(0)?, row.try_get::<_, i64>(1)?)))
                        }
                        None => Ok(None),
                    }
                })
                .await
            }
            .await;
            match outcome {
                Ok(row) => break row,
                Err(failure) => Self::between(&mut attempt, failure).await?,
            }
        };
        row.map(|(lane, seq)| lane_seq(lane, seq, "event index"))
            .transpose()
    }

    async fn store_event(&self, event_id: &str, lane: u32, seq: u64) -> Result<()> {
        let what = "record event index";
        let want = (i64::from(lane), signed(seq, "event index")?);
        let mut attempt = 1;
        let stored = loop {
            let outcome = async {
                let client = self.checkout(what).await?;
                self.run_once(what, async {
                    client
                        .execute(EVENT_INSERT, &[&self.site, &event_id, &want.0, &want.1])
                        .await?;
                    // Read back what is stored: our row, or the one that was there first. The
                    // insert has returned, so this statement sees it either way.
                    let row = client
                        .query_one(EVENT_GET, &[&self.site, &event_id])
                        .await?;
                    Ok((row.try_get::<_, i64>(0)?, row.try_get::<_, i64>(1)?))
                })
                .await
            }
            .await;
            match outcome {
                Ok(stored) => break stored,
                Err(failure) => Self::between(&mut attempt, failure).await?,
            }
        };
        if stored == want {
            Ok(())
        } else {
            Err(Error::new(
                EVENT_CONFLICT,
                "an event id is already recorded at a different lane and sequence; the stored \
                 entry was kept",
            ))
        }
    }

    async fn events_of(&self, ids: &[String]) -> Result<Vec<Option<(u32, u64)>>> {
        let what = "read event index";
        let mut attempt = 1;
        let rows = loop {
            let outcome = async {
                let client = self.checkout(what).await?;
                self.run_once(what, async {
                    let statement = client.prepare_cached(EVENT_GET_MANY).await?;
                    let rows = client.query(&statement, &[&self.site, &ids]).await?;
                    let mut out = std::collections::HashMap::with_capacity(rows.len());
                    for row in &rows {
                        out.insert(
                            row.try_get::<_, String>(0)?,
                            (row.try_get::<_, i64>(1)?, row.try_get::<_, i64>(2)?),
                        );
                    }
                    Ok(out)
                })
                .await
            }
            .await;
            match outcome {
                Ok(rows) => break rows,
                Err(failure) => Self::between(&mut attempt, failure).await?,
            }
        };
        ids.iter()
            .map(|id| {
                rows.get(id)
                    .map(|(lane, seq)| lane_seq(*lane, *seq, "event index"))
                    .transpose()
            })
            .collect()
    }

    /// The document entries, then the event ids, in one transaction.
    async fn store_many(
        &self,
        documents: &[spot_doc_audit::DocumentEntry],
        events: &[(String, u32, u64)],
    ) -> Result<()> {
        if documents.is_empty() && events.is_empty() {
            return Ok(());
        }
        let what = "record index rows";
        let doctypes: Vec<String> = documents.iter().map(|d| d.0.clone()).collect();
        let doc_ids: Vec<String> = documents.iter().map(|d| d.1.clone()).collect();
        let mut doc_lanes = Vec::with_capacity(documents.len());
        let mut doc_seqs = Vec::with_capacity(documents.len());
        for (_, _, lane, seq) in documents {
            doc_lanes.push(i64::from(*lane));
            doc_seqs.push(signed(*seq, "document index")?);
        }
        let event_ids: Vec<String> = events.iter().map(|e| e.0.clone()).collect();
        let mut event_lanes = Vec::with_capacity(events.len());
        let mut event_seqs = Vec::with_capacity(events.len());
        for (_, lane, seq) in events {
            event_lanes.push(i64::from(*lane));
            event_seqs.push(signed(*seq, "event index")?);
        }
        let mut attempt = 1;
        let stored = loop {
            let outcome = async {
                let mut client = self.checkout(what).await?;
                self.run_once(what, async {
                    let tx = client.transaction().await?;
                    if !documents.is_empty() {
                        let statement = tx.prepare_cached(DOC_INSERT_MANY).await?;
                        tx.execute(
                            &statement,
                            &[&self.site, &doctypes, &doc_ids, &doc_lanes, &doc_seqs],
                        )
                        .await?;
                    }
                    let mut stored = std::collections::HashMap::new();
                    if !events.is_empty() {
                        let statement = tx.prepare_cached(EVENT_INSERT_MANY).await?;
                        tx.execute(
                            &statement,
                            &[&self.site, &event_ids, &event_lanes, &event_seqs],
                        )
                        .await?;
                        // Read back what is stored: our rows, or the ones that were there first.
                        let statement = tx.prepare_cached(EVENT_GET_MANY).await?;
                        for row in tx.query(&statement, &[&self.site, &event_ids]).await? {
                            stored.insert(
                                row.try_get::<_, String>(0)?,
                                (row.try_get::<_, i64>(1)?, row.try_get::<_, i64>(2)?),
                            );
                        }
                    }
                    tx.commit().await?;
                    Ok(stored)
                })
                .await
            }
            .await;
            match outcome {
                Ok(stored) => break stored,
                Err(failure) => Self::between(&mut attempt, failure).await?,
            }
        };
        for ((id, _, _), want) in events.iter().zip(event_lanes.iter().zip(&event_seqs)) {
            if stored.get(id) != Some(&(*want.0, *want.1)) {
                return Err(Error::new(
                    EVENT_CONFLICT,
                    "an event id is already recorded at a different lane and sequence; the \
                     stored entry was kept",
                ));
            }
        }
        Ok(())
    }

    async fn entries_of(&self, doctype: &str, id: &str) -> Result<Vec<(u32, u64)>> {
        let what = "read document index";
        let mut attempt = 1;
        let rows = loop {
            let outcome = async {
                let client = self.checkout(what).await?;
                self.run_once(what, async {
                    let rows = client.query(DOC_LIST, &[&self.site, &doctype, &id]).await?;
                    let mut out = Vec::with_capacity(rows.len());
                    for row in &rows {
                        out.push((row.try_get::<_, i64>(0)?, row.try_get::<_, i64>(1)?));
                    }
                    Ok(out)
                })
                .await
            }
            .await;
            match outcome {
                Ok(rows) => break rows,
                Err(failure) => Self::between(&mut attempt, failure).await?,
            }
        };
        rows.into_iter()
            .map(|(lane, seq)| lane_seq(lane, seq, "document index"))
            .collect()
    }

    async fn store_entry(&self, doctype: &str, id: &str, lane: u32, seq: u64) -> Result<()> {
        let what = "record document index";
        let (lane, seq) = (i64::from(lane), signed(seq, "document index")?);
        let mut attempt = 1;
        loop {
            let outcome = async {
                let client = self.checkout(what).await?;
                self.run_once(what, async {
                    client
                        .execute(DOC_INSERT, &[&self.site, &doctype, &id, &lane, &seq])
                        .await
                        .map(|_| ())
                })
                .await
            }
            .await;
            match outcome {
                Ok(()) => return Ok(()),
                Err(failure) => Self::between(&mut attempt, failure).await?,
            }
        }
    }

    async fn checkpoint_of(&self) -> Result<Option<SiteCheckpoint>> {
        let what = "read site checkpoint";
        let mut attempt = 1;
        let body = loop {
            let outcome = async {
                let client = self.checkout(what).await?;
                self.run_once(what, async {
                    match client.query_opt(CHECKPOINT_GET, &[&self.site]).await? {
                        Some(row) => Ok(Some(row.try_get::<_, String>(0)?)),
                        None => Ok(None),
                    }
                })
                .await
            }
            .await;
            match outcome {
                Ok(body) => break body,
                Err(failure) => Self::between(&mut attempt, failure).await?,
            }
        };
        body.map(|text| {
            serde_json::from_str::<SiteCheckpoint>(&text)
                .map_err(|_| corrupt("the site checkpoint does not decode"))
        })
        .transpose()
    }

    async fn store_checkpoint(&self, checkpoint: &SiteCheckpoint) -> Result<()> {
        let what = "store site checkpoint";
        let at = checkpoint.at;
        let body = serde_json::to_string(checkpoint)
            .map_err(|_| Error::unavailable("the site checkpoint could not be encoded"))?;
        let mut attempt = 1;
        loop {
            let outcome = async {
                let client = self.checkout(what).await?;
                self.run_once(what, async {
                    client
                        .execute(CHECKPOINT_SET, &[&self.site, &at, &body])
                        .await
                        .map(|_| ())
                })
                .await
            }
            .await;
            match outcome {
                Ok(()) => return Ok(()),
                Err(failure) => Self::between(&mut attempt, failure).await?,
            }
        }
    }
}

impl AppenderStore for PgAppenderStore {
    fn bookmark(&self, lane: u32) -> StateFuture<'_, FeedPosition> {
        Box::pin(async move {
            self.bookmark_of(lane)
                .await
                .map_err(|e| state_error("bookmark", &e))
        })
    }

    fn set_bookmark(&self, lane: u32, position: FeedPosition) -> StateFuture<'_, ()> {
        Box::pin(async move {
            self.store_bookmark(lane, position)
                .await
                .map_err(|e| state_error("set_bookmark", &e))
        })
    }

    fn appended<'a>(&'a self, event_id: &'a str) -> StateFuture<'a, Option<(u32, u64)>> {
        Box::pin(async move {
            self.event_of(event_id)
                .await
                .map_err(|e| state_error("appended", &e))
        })
    }

    fn record_appended<'a>(
        &'a self,
        event_id: &'a str,
        lane: u32,
        seq: u64,
    ) -> StateFuture<'a, ()> {
        Box::pin(async move {
            self.store_event(event_id, lane, seq)
                .await
                .map_err(|e| state_error("record_appended", &e))
        })
    }

    fn document_entries<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
    ) -> StateFuture<'a, Vec<(u32, u64)>> {
        Box::pin(async move {
            self.entries_of(doctype, id)
                .await
                .map_err(|e| state_error("document_entries", &e))
        })
    }

    fn record_document_entry<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
        lane: u32,
        seq: u64,
    ) -> StateFuture<'a, ()> {
        Box::pin(async move {
            self.store_entry(doctype, id, lane, seq)
                .await
                .map_err(|e| state_error("record_document_entry", &e))
        })
    }

    fn batches(&self) -> bool {
        true
    }

    fn appended_many<'a>(
        &'a self,
        event_ids: &'a [String],
    ) -> StateFuture<'a, Vec<Option<(u32, u64)>>> {
        Box::pin(async move {
            self.events_of(event_ids)
                .await
                .map_err(|e| state_error("appended_many", &e))
        })
    }

    fn record_many<'a>(
        &'a self,
        documents: &'a [spot_doc_audit::DocumentEntry],
        events: &'a [(String, u32, u64)],
    ) -> StateFuture<'a, ()> {
        Box::pin(async move {
            self.store_many(documents, events)
                .await
                .map_err(|e| state_error("record_many", &e))
        })
    }

    fn site_checkpoint(&self) -> StateFuture<'_, Option<SiteCheckpoint>> {
        Box::pin(async move {
            self.checkpoint_of()
                .await
                .map_err(|e| state_error("site_checkpoint", &e))
        })
    }

    fn set_site_checkpoint(&self, checkpoint: SiteCheckpoint) -> StateFuture<'_, ()> {
        Box::pin(async move {
            self.store_checkpoint(&checkpoint)
                .await
                .map_err(|e| state_error("set_site_checkpoint", &e))
        })
    }
}

/// A store error as the appender's typed error: the operation and the store's code, nothing else.
fn state_error(operation: &str, error: &Error) -> DocAuditError {
    DocAuditError::appender_state(operation, error.code)
}

/// Whether a database failure is a connection-level one a repeat can cure: the connection is
/// gone, or the server answered with no SQLSTATE at all. A refusal that carries a SQLSTATE is a
/// verdict, not a hiccup.
fn retryable(error: &PgError) -> bool {
    error.is_closed() || error.code().is_none()
}

/// A stored non-negative `bigint` as `u64`.
fn unsigned(value: i64, what: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| corrupt(&format!("a negative number in the {what}")))
}

/// A `u64` as a `bigint` value.
fn signed(value: u64, what: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| Error::unavailable(format!("a number in the {what} does not fit the column")))
}

/// A stored `(lane, seq)` pair.
fn lane_seq(lane: i64, seq: i64, what: &str) -> Result<(u32, u64)> {
    let lane = u32::try_from(lane)
        .map_err(|_| corrupt(&format!("a lane outside the range in the {what}")))?;
    Ok((lane, unsigned(seq, what)?))
}
