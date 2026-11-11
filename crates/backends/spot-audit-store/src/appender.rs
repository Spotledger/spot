//! [`PgAppenderStore`]: the lane appender's durable state in PostgreSQL (WP-AI-11.T5).
//!
//! `spot_doc_audit::LaneAppender` keeps four things in an [`AppenderStore`]: each lane's outbox
//! bookmark, the event-id index that makes a re-delivered event chain once, the per-document
//! index the review and the digest read, and the newest signed site checkpoint. The in-memory
//! reference forgets them on a restart, which re-chains every event; this store keeps them in
//! four `spot_audit_*` tables, every key starting with the site, so a store for site A can never
//! read or write site B's rows.
//!
//! # Synchronous, over the blocking bridge
//!
//! The trait is synchronous, so every call goes through the one shared
//! [`spot_pg_bridge::Bridge`] (ADR-082 addendum 1, class C). The bridge refuses a call from a
//! thread inside a tokio runtime with `pg_bridge.wrong_thread`; that refusal is a failure of this
//! store like any other (below), never a silent default. See the crate's open questions: the
//! appender's `drain` is `async`, so an `AppenderStore` it calls from a runtime thread is
//! refused.
//!
//! # Failure is loud, and the store fails closed
//!
//! The trait methods return no `Result`, and a database failure must not be answered as "not
//! found" (that re-chains an event) nor as success. So every method has a fallible `try_*`
//! twin that returns the error with its stable code, and the trait methods **latch** the first
//! failure ([`PgAppenderStore::failure`]) and, while a failure is latched, behave so the appender
//! cannot make progress or lose an event:
//!
//! - [`AppenderStore::bookmark`] returns [`poison_position`], a position no feed accepts, so the
//!   next `drain` fails at its feed read (`ResyncRequired`) and the appender job fails and is
//!   retried;
//! - [`AppenderStore::appended`] answers "already chained" with the sentinel `(u32::MAX,
//!   u64::MAX)`, so no event is chained while the index cannot be read; the bookmark does not
//!   advance (next point), so the event is delivered again once the failure is cleared;
//! - every write is a no-op while a failure is latched, in particular
//!   [`AppenderStore::set_bookmark`]: the bookmark never moves past an event whose index entry
//!   was not stored;
//! - [`AppenderStore::document_entries`] and [`AppenderStore::site_checkpoint`] answer empty (they
//!   only feed views and a checkpoint verification).
//!
//! The host (or the job) calls [`PgAppenderStore::take_failure`] after a failed drain, reports the
//! error and, when it is safe to go on, clears the latch. A restart also clears it, because the
//! latch lives in the process while every fact the appender relies on lives in the database.
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

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use spot_doc_audit::{AppenderStore, SiteCheckpoint};
use spot_pg_bridge::{BoxFuture, Bridge, BridgeError, PooledConn};
use spot_storage_contract::feed::FeedPosition;
use tokio_postgres::Error as PgError;

use crate::appender_sql::{
    BOOKMARK_GET, BOOKMARK_SET, CHECKPOINT_GET, CHECKPOINT_SET, DOC_INSERT, DOC_LIST, EVENT_GET,
    EVENT_INSERT,
};
use crate::error::{EVENT_CONFLICT, Error, Result, corrupt, db};

/// The deadline of one database call unless [`PgAppenderStore::with_deadline`] sets another.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(5);

/// How many times a call is tried when the bridge says retrying can help (busy, timeout,
/// connect). Every statement is an idempotent upsert or a read, so a repeat is safe.
const ATTEMPTS: u32 = 3;

/// The pause between two attempts.
const RETRY_PAUSE: Duration = Duration::from_millis(25);

/// The bookmark [`AppenderStore::bookmark`] returns while a failure is latched: a generation no
/// backend has, so a feed read from it is refused instead of answered with an empty page.
#[must_use]
pub fn poison_position() -> FeedPosition {
    FeedPosition::new(u64::MAX, u64::MAX)
}

/// The `(lane, seq)` [`AppenderStore::appended`] answers while a failure is latched.
const FAILED_CLOSED: (u32, u64) = (u32::MAX, u64::MAX);

/// The appender state of one site in PostgreSQL. See the module documentation.
pub struct PgAppenderStore {
    bridge: Arc<Bridge>,
    site: String,
    deadline: Duration,
    failure: Mutex<Option<Error>>,
}

impl PgAppenderStore {
    /// A store for `site` over the process's shared `bridge`, with [`DEFAULT_DEADLINE`].
    ///
    /// Nothing is read or created: the tables come from the migrator ([`crate::APPENDER_TABLE_DDL`])
    /// and a host checks them with [`crate::verify_appender`] before it serves the site.
    #[must_use]
    pub fn new(bridge: Arc<Bridge>, site: impl Into<String>) -> Self {
        Self {
            bridge,
            site: site.into(),
            deadline: DEFAULT_DEADLINE,
            failure: Mutex::new(None),
        }
    }

    /// The same store with `deadline` as the time budget of one database call.
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

    /// The first failure a trait method latched, if any. The store stays failed closed until
    /// [`PgAppenderStore::take_failure`].
    #[must_use]
    pub fn failure(&self) -> Option<Error> {
        self.latch().clone()
    }

    /// Returns the latched failure and clears it, so the store answers from the database again.
    pub fn take_failure(&self) -> Option<Error> {
        self.latch().take()
    }

    fn latch(&self) -> MutexGuard<'_, Option<Error>> {
        self.failure.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn failed(&self) -> bool {
        self.latch().is_some()
    }

    /// Keeps the first failure; a later one is a consequence of it.
    fn record_failure(&self, error: Error) {
        let mut slot = self.latch();
        if slot.is_none() {
            *slot = Some(error);
        }
    }

    /// Runs `op` over the bridge, retrying only what the bridge calls retryable.
    fn call<T, F>(&self, what: &'static str, op: F) -> Result<T>
    where
        T: Send + 'static,
        F: Fn(PooledConn) -> BoxFuture<'static, std::result::Result<T, PgError>>
            + Clone
            + Send
            + 'static,
    {
        let mut attempt = 1;
        loop {
            match self.bridge.run(self.deadline, op.clone()) {
                Ok(value) => return Ok(value),
                Err(error) if error.is_retryable() && attempt < ATTEMPTS => {
                    attempt += 1;
                    std::thread::sleep(RETRY_PAUSE);
                }
                Err(error) => return Err(bridge_error(what, error)),
            }
        }
    }

    /// The lane's bookmark; [`FeedPosition::START`] when none was stored.
    ///
    /// # Errors
    ///
    /// [`crate::UNAVAILABLE`] (or the bridge's `wrong_thread`) when the database cannot answer.
    pub fn try_bookmark(&self, lane: u32) -> Result<FeedPosition> {
        let site = self.site.clone();
        let lane = i64::from(lane);
        let row = self.call("read bookmark", move |conn| {
            let site = site.clone();
            Box::pin(async move {
                match conn.query_opt(BOOKMARK_GET, &[&site, &lane]).await? {
                    Some(row) => Ok(Some((row.try_get::<_, i64>(0)?, row.try_get::<_, i64>(1)?))),
                    None => Ok(None),
                }
            })
        })?;
        match row {
            None => Ok(FeedPosition::START),
            Some((generation, seq)) => Ok(FeedPosition::new(
                unsigned(generation, "bookmark")?,
                unsigned(seq, "bookmark")?,
            )),
        }
    }

    /// Stores the lane's bookmark.
    ///
    /// # Errors
    ///
    /// [`crate::UNAVAILABLE`] when the database cannot store it or the position does not fit the
    /// column.
    pub fn try_set_bookmark(&self, lane: u32, position: FeedPosition) -> Result<()> {
        let site = self.site.clone();
        let lane = i64::from(lane);
        let generation = signed(position.generation(), "bookmark")?;
        let seq = signed(position.seq(), "bookmark")?;
        self.call("store bookmark", move |conn| {
            let site = site.clone();
            Box::pin(async move {
                conn.execute(BOOKMARK_SET, &[&site, &lane, &generation, &seq])
                    .await
                    .map(|_| ())
            })
        })
    }

    /// The `(lane, seq)` the event id was chained at, if recorded.
    ///
    /// # Errors
    ///
    /// [`crate::UNAVAILABLE`] when the database cannot answer: never "not recorded".
    pub fn try_appended(&self, event_id: &str) -> Result<Option<(u32, u64)>> {
        let site = self.site.clone();
        let event_id = event_id.to_owned();
        let row = self.call("read event index", move |conn| {
            let (site, event_id) = (site.clone(), event_id.clone());
            Box::pin(async move {
                match conn.query_opt(EVENT_GET, &[&site, &event_id]).await? {
                    Some(row) => Ok(Some((row.try_get::<_, i64>(0)?, row.try_get::<_, i64>(1)?))),
                    None => Ok(None),
                }
            })
        })?;
        row.map(|(lane, seq)| lane_seq(lane, seq, "event index"))
            .transpose()
    }

    /// Records that the event id was chained at `(lane, seq)`.
    ///
    /// # Errors
    ///
    /// [`EVENT_CONFLICT`] when the id is already recorded at a different `(lane, seq)` (the stored
    /// row is kept); [`crate::UNAVAILABLE`] when the database cannot store it.
    pub fn try_record_appended(&self, event_id: &str, lane: u32, seq: u64) -> Result<()> {
        let site = self.site.clone();
        let event_id = event_id.to_owned();
        let want = (i64::from(lane), signed(seq, "event index")?);
        let stored = self.call("record event index", move |conn| {
            let (site, event_id) = (site.clone(), event_id.clone());
            Box::pin(async move {
                conn.execute(EVENT_INSERT, &[&site, &event_id, &want.0, &want.1])
                    .await?;
                // Read back what is stored: our row, or the one that was there first. The
                // insert has returned, so this statement sees it either way.
                let row = conn.query_one(EVENT_GET, &[&site, &event_id]).await?;
                Ok((row.try_get::<_, i64>(0)?, row.try_get::<_, i64>(1)?))
            })
        })?;
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

    /// The `(lane, seq)` entries of one document, in the order they were recorded.
    ///
    /// # Errors
    ///
    /// [`crate::UNAVAILABLE`] when the database cannot answer.
    pub fn try_document_entries(&self, doctype: &str, id: &str) -> Result<Vec<(u32, u64)>> {
        let site = self.site.clone();
        let (doctype, id) = (doctype.to_owned(), id.to_owned());
        let rows = self.call("read document index", move |conn| {
            let (site, doctype, id) = (site.clone(), doctype.clone(), id.clone());
            Box::pin(async move {
                let rows = conn.query(DOC_LIST, &[&site, &doctype, &id]).await?;
                let mut out = Vec::with_capacity(rows.len());
                for row in &rows {
                    out.push((row.try_get::<_, i64>(0)?, row.try_get::<_, i64>(1)?));
                }
                Ok(out)
            })
        })?;
        rows.into_iter()
            .map(|(lane, seq)| lane_seq(lane, seq, "document index"))
            .collect()
    }

    /// Records one `(lane, seq)` entry for a document; recording it again changes nothing.
    ///
    /// # Errors
    ///
    /// [`crate::UNAVAILABLE`] when the database cannot store it.
    pub fn try_record_document_entry(
        &self,
        doctype: &str,
        id: &str,
        lane: u32,
        seq: u64,
    ) -> Result<()> {
        let site = self.site.clone();
        let (doctype, id) = (doctype.to_owned(), id.to_owned());
        let (lane, seq) = (i64::from(lane), signed(seq, "document index")?);
        self.call("record document index", move |conn| {
            let (site, doctype, id) = (site.clone(), doctype.clone(), id.clone());
            Box::pin(async move {
                conn.execute(DOC_INSERT, &[&site, &doctype, &id, &lane, &seq])
                    .await
                    .map(|_| ())
            })
        })
    }

    /// The newest signed site checkpoint, if any.
    ///
    /// # Errors
    ///
    /// [`crate::UNAVAILABLE`] when the database cannot answer or the stored checkpoint does not
    /// decode.
    pub fn try_site_checkpoint(&self) -> Result<Option<SiteCheckpoint>> {
        let site = self.site.clone();
        let body = self.call("read site checkpoint", move |conn| {
            let site = site.clone();
            Box::pin(async move {
                match conn.query_opt(CHECKPOINT_GET, &[&site]).await? {
                    Some(row) => Ok(Some(row.try_get::<_, String>(0)?)),
                    None => Ok(None),
                }
            })
        })?;
        body.map(|text| {
            serde_json::from_str::<SiteCheckpoint>(&text)
                .map_err(|_| corrupt("the site checkpoint does not decode"))
        })
        .transpose()
    }

    /// Stores a signed site checkpoint. A checkpoint older than the stored one is ignored.
    ///
    /// # Errors
    ///
    /// [`crate::UNAVAILABLE`] when the database cannot store it.
    pub fn try_set_site_checkpoint(&self, checkpoint: &SiteCheckpoint) -> Result<()> {
        let site = self.site.clone();
        let at = checkpoint.at;
        let body = serde_json::to_string(checkpoint)
            .map_err(|_| Error::unavailable("the site checkpoint could not be encoded"))?;
        self.call("store site checkpoint", move |conn| {
            let (site, body) = (site.clone(), body.clone());
            Box::pin(async move {
                conn.execute(CHECKPOINT_SET, &[&site, &at, &body])
                    .await
                    .map(|_| ())
            })
        })
    }
}

impl AppenderStore for PgAppenderStore {
    fn bookmark(&self, lane: u32) -> FeedPosition {
        if self.failed() {
            return poison_position();
        }
        self.try_bookmark(lane).unwrap_or_else(|error| {
            self.record_failure(error);
            poison_position()
        })
    }

    fn set_bookmark(&self, lane: u32, position: FeedPosition) {
        if self.failed() {
            return;
        }
        if let Err(error) = self.try_set_bookmark(lane, position) {
            self.record_failure(error);
        }
    }

    fn appended(&self, event_id: &str) -> Option<(u32, u64)> {
        if self.failed() {
            return Some(FAILED_CLOSED);
        }
        self.try_appended(event_id).unwrap_or_else(|error| {
            self.record_failure(error);
            Some(FAILED_CLOSED)
        })
    }

    fn record_appended(&self, event_id: &str, lane: u32, seq: u64) {
        if self.failed() {
            return;
        }
        if let Err(error) = self.try_record_appended(event_id, lane, seq) {
            self.record_failure(error);
        }
    }

    fn document_entries(&self, doctype: &str, id: &str) -> Vec<(u32, u64)> {
        if self.failed() {
            return Vec::new();
        }
        self.try_document_entries(doctype, id)
            .unwrap_or_else(|error| {
                self.record_failure(error);
                Vec::new()
            })
    }

    fn record_document_entry(&self, doctype: &str, id: &str, lane: u32, seq: u64) {
        if self.failed() {
            return;
        }
        if let Err(error) = self.try_record_document_entry(doctype, id, lane, seq) {
            self.record_failure(error);
        }
    }

    fn site_checkpoint(&self) -> Option<SiteCheckpoint> {
        if self.failed() {
            return None;
        }
        self.try_site_checkpoint().unwrap_or_else(|error| {
            self.record_failure(error);
            None
        })
    }

    fn set_site_checkpoint(&self, checkpoint: SiteCheckpoint) {
        if self.failed() {
            return;
        }
        if let Err(error) = self.try_set_site_checkpoint(&checkpoint) {
            self.record_failure(error);
        }
    }
}

/// A bridge refusal as a store error. The wrong-thread refusal keeps its own stable code (it is a
/// defect in the caller, not an outage); everything else is `auditstore.unavailable` with the
/// bridge's code in the message.
fn bridge_error(what: &str, error: BridgeError<PgError>) -> Error {
    match error {
        BridgeError::Store(e) => db(what, &e),
        BridgeError::WrongThread => Error::new(
            spot_pg_bridge::WRONG_THREAD,
            format!(
                "{what}: the audit appender state was called from a thread inside an async runtime"
            ),
        ),
        other => Error::unavailable(format!(
            "{what}: the bridge refused the call ({})",
            other.code().unwrap_or("unknown")
        )),
    }
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
