//! The held session: one bridge call, one database connection, served by one thread.
//!
//! The live side must read its whole answer from **one** transaction (ADR-061 s8): the head, the
//! record summaries and the fingerprints all come from the snapshot it began. `spot-pg-bridge`
//! hands a connection to a closure and takes it back when the call returns, and a call's deadline
//! is capped by the bridge's `max_deadline`, so a transaction cannot be held *across* calls: the
//! side therefore keeps **one call open for its whole life**. [`Session::open`] starts a thread
//! that runs that call; the closure begins the transaction, reports the head, and then serves
//! requests until the side is dropped, when it rolls back.
//!
//! Requests travel one way (an unbounded channel the closure awaits, so a waiting worker costs no
//! runtime thread) and answers travel back on a channel per request, which is also the back
//! pressure: the side asks for one bounded page at a time and never holds more.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender as StdSender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use spot_journal_audit::{Fingerprint, RecordSummary};
use spot_pg_bridge::{BoxFuture, Bridge, PooledConn};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::error::{PgSideError, session_failed};

/// What a closure reports once, before it serves anything: the live side's head position, and
/// nothing at all for the scratch side.
pub(crate) type HeadReport<T> = StdSender<Result<T, PgSideError>>;

/// One request of a side.
pub(crate) enum Request {
    /// One image the rebuild applies to the scratch schema.
    ApplyImage {
        /// The image to store.
        image: spot_journal_rebuild::AppliedImage,
        /// Answered once the image and its child rows are committed.
        reply: Reply<()>,
    },
    /// One tombstone the rebuild applies to the scratch schema.
    ApplyDelete {
        /// The entity.
        entity: String,
        /// The record id.
        id: String,
        /// Answered once the record and its child rows are removed.
        reply: Reply<()>,
    },
    /// Empties every scratch table (an aborted rebuild).
    Truncate {
        /// Answered once the tables are empty.
        reply: Reply<()>,
    },
    /// Drops the scratch schema.
    Discard {
        /// Answered once the schema is gone.
        reply: Reply<()>,
    },
    /// One keyset page of `entity`, after `after` (`None` for the first page).
    Page {
        /// The entity's name.
        entity: String,
        /// The last id of the previous page.
        after: Option<String>,
        /// Rows at most.
        limit: i64,
        /// Where the page goes.
        reply: Reply<Vec<RecordSummary>>,
    },
    /// Every declared fingerprint.
    Fingerprints {
        /// Where the fingerprints go.
        reply: Reply<Vec<Fingerprint>>,
    },
    /// No more requests: the worker rolls back and the call returns.
    Close {
        /// Answered before the worker exits.
        reply: Reply<()>,
    },
    /// Runs one statement and reports the SQLSTATE of its refusal (a test hook).
    #[cfg(feature = "testing")]
    Probe {
        /// The statement.
        sql: String,
        /// The SQLSTATE, or an empty string when it went through.
        reply: Reply<String>,
    },
}

/// The answer channel of one request.
pub(crate) struct Reply<T>(StdSender<Result<T, PgSideError>>);

impl<T> Reply<T> {
    /// Answers the waiting caller; a caller that gave up is not an error.
    pub(crate) fn send(self, value: Result<T, PgSideError>) {
        let _ = self.0.send(value);
    }
}

/// One held session: the request channel, the thread that runs the call, and what went wrong.
pub(crate) struct Session {
    requests: UnboundedSender<Request>,
    thread: Option<std::thread::JoinHandle<()>>,
    failure: Arc<Mutex<Option<PgSideError>>>,
    pages: Arc<AtomicUsize>,
}

impl Session {
    /// Runs `serve` inside one bridge call on its own thread and returns the session with the
    /// position the closure reported.
    ///
    /// # Errors
    ///
    /// The closure's own refusal when it cannot begin, a [`SESSION_FAILED`](crate::error::
    /// SESSION_FAILED) when the bridge refused the call or the worker died before answering.
    pub(crate) fn open<T, F>(
        bridge: &Arc<Bridge>,
        deadline: Duration,
        serve: F,
    ) -> Result<(Self, T), PgSideError>
    where
        T: Send + 'static,
        F: FnOnce(
                PooledConn,
                UnboundedReceiver<Request>,
                HeadReport<T>,
            ) -> BoxFuture<'static, Result<(), PgSideError>>
            + Send
            + 'static,
    {
        let (requests, receiver) = unbounded_channel();
        let (head_tx, head_rx) = channel();
        let failure: Arc<Mutex<Option<PgSideError>>> = Arc::new(Mutex::new(None));
        let pages = Arc::new(AtomicUsize::new(0));
        let shared = Arc::clone(&failure);
        let bridge = Arc::clone(bridge);
        let thread = std::thread::Builder::new()
            .name("spot-journal-audit-pg".to_owned())
            .spawn(move || {
                let result = bridge.run(deadline, move |conn| serve(conn, receiver, head_tx));
                if let Err(error) = result {
                    note(&shared, session_failed(&error));
                }
            })
            .map_err(|_| {
                crate::error::error(
                    crate::error::SESSION_FAILED,
                    "the side's session thread could not be started",
                )
            })?;
        let mut session = Session {
            requests,
            thread: Some(thread),
            failure,
            pages,
        };
        match head_rx.recv() {
            Ok(Ok(head)) => Ok((session, head)),
            Ok(Err(error)) => {
                session.close();
                Err(error)
            }
            // The worker ended before it could report: its reason is recorded above.
            Err(_) => {
                let recorded = session.failure();
                session.close();
                Err(recorded.unwrap_or_else(|| {
                    crate::error::error(
                        crate::error::SESSION_FAILED,
                        "the held session ended before it read the head",
                    )
                }))
            }
        }
    }

    /// One page of summaries. The call blocks until the worker answers.
    pub(crate) fn page(
        &self,
        entity: &str,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Vec<RecordSummary>, PgSideError> {
        self.pages.fetch_add(1, Ordering::SeqCst);
        self.ask(|reply| Request::Page {
            entity: entity.to_owned(),
            after: after.map(str::to_owned),
            limit,
            reply,
        })
    }

    /// Applies one image to the scratch schema.
    pub(crate) fn apply_image(
        &self,
        image: spot_journal_rebuild::AppliedImage,
    ) -> Result<(), PgSideError> {
        self.ask(move |reply| Request::ApplyImage { image, reply })
    }

    /// Applies one tombstone to the scratch schema.
    pub(crate) fn apply_delete(&self, entity: &str, id: &str) -> Result<(), PgSideError> {
        self.ask(|reply| Request::ApplyDelete {
            entity: entity.to_owned(),
            id: id.to_owned(),
            reply,
        })
    }

    /// Empties every scratch table.
    pub(crate) fn truncate(&self) -> Result<(), PgSideError> {
        self.ask(|reply| Request::Truncate { reply })
    }

    /// Drops the scratch schema.
    pub(crate) fn discard_schema(&self) -> Result<(), PgSideError> {
        self.ask(|reply| Request::Discard { reply })
    }

    /// Runs one statement and reports the SQLSTATE of its refusal (a test hook).
    #[cfg(feature = "testing")]
    pub(crate) fn probe(&self, sql: &str) -> Result<String, PgSideError> {
        self.ask(|reply| Request::Probe {
            sql: sql.to_owned(),
            reply,
        })
    }

    /// Every fingerprint the scope can answer.
    pub(crate) fn fingerprints(&self) -> Result<Vec<Fingerprint>, PgSideError> {
        self.ask(|reply| Request::Fingerprints { reply })
    }

    /// How many pages this session has been asked for (a bounded read asks for one at a time).
    pub(crate) fn pages_read(&self) -> usize {
        self.pages.load(Ordering::SeqCst)
    }

    /// The first failure recorded by this side, if any.
    pub(crate) fn failure(&self) -> Option<PgSideError> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Records a failure (a refusal of a call the traits cannot answer with one).
    pub(crate) fn record(&self, error: &PgSideError) {
        note(&self.failure, error.clone());
    }

    /// Asks the worker one question and waits for its answer.
    pub(crate) fn ask<T>(&self, build: impl FnOnce(Reply<T>) -> Request) -> Result<T, PgSideError> {
        let (tx, rx) = channel();
        if self.requests.send(build(Reply(tx))).is_err() {
            return Err(self.ended());
        }
        rx.recv().unwrap_or_else(|_| Err(self.ended()))
    }

    /// Ends the held call: the worker rolls back and its thread is joined.
    pub(crate) fn close(&mut self) {
        let (tx, rx) = channel();
        if self
            .requests
            .send(Request::Close { reply: Reply(tx) })
            .is_ok()
        {
            let _ = rx.recv();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Why the session is gone: what was recorded, or a plain `SESSION_FAILED`.
    fn ended(&self) -> PgSideError {
        self.failure().unwrap_or_else(|| {
            crate::error::error(
                crate::error::SESSION_FAILED,
                "the held session is closed and cannot answer",
            )
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

/// Keeps the first recorded failure: a later one is a consequence of it.
fn note(slot: &Arc<Mutex<Option<PgSideError>>>, error: PgSideError) {
    let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(error);
    }
}

/// The receiver a worker awaits, so the loop is written the same way in both sides.
pub(crate) type Requests = UnboundedReceiver<Request>;

/// A helper for the worker loops: the next request, or `None` when the side was dropped.
pub(crate) async fn next_request(requests: &mut Requests) -> Option<Request> {
    requests.recv().await
}

/// Runs one short bridge call on its own thread and returns its answer.
///
/// The host of the audit drives the runner inside a tokio runtime context, and the bridge refuses
/// a call made from a runtime thread (`pg_bridge.wrong_thread`). Every statement of these sides
/// therefore runs on a plain thread the side owns — this one for a script's questions (the sweep,
/// a schema probe), [`Session`]'s for a side's life.
pub(crate) fn run_blocking<T, F>(
    bridge: &Arc<Bridge>,
    deadline: Duration,
    op: F,
) -> Result<T, PgSideError>
where
    T: Send + 'static,
    F: FnOnce(PooledConn) -> BoxFuture<'static, Result<T, PgSideError>> + Send + 'static,
{
    let bridge = Arc::clone(bridge);
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("spot-journal-audit-pg-script".to_owned())
        .spawn(move || {
            let result = bridge.run(deadline, op);
            let _ = tx.send(result);
        })
        .map_err(|_| {
            crate::error::error(
                crate::error::SESSION_FAILED,
                "the side's thread could not be started",
            )
        })?;
    match rx.recv() {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(session_failed(&error)),
        Err(_) => Err(crate::error::error(
            crate::error::SESSION_FAILED,
            "the side's thread ended without an answer",
        )),
    }
}
