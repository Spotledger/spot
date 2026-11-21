//! The PostgreSQL denial writer (WP-AI-11.T6): the [`DenialWriter`] that records one refused
//! operation in the `sys_doc_audit` outbox, in a transaction of its own.
//!
//! A refusal happens after the operation's transaction has rolled back, so the audit denial sink
//! ([`spot_doc_audit::AuditDenialSink::with_denial_writer`]) cannot reuse it. [`PgDenialWriter`]
//! begins a new transaction on the site's storage backend, writes the one outbox event the audit
//! step would write for an ordinary event ([`spot_doc_audit::step::outbox_event_of`], so the lane
//! appender chains it like any other) and commits. A refused operation therefore leaves exactly
//! one denial row and no document row.
//!
//! # Scope and secrecy
//!
//! - The writer belongs to one site. An event for another site is refused with
//!   [`SITE_MISMATCH`] and nothing is written.
//! - The event is written as it is given: the sink builds it from the error's class and code
//!   only, so it holds no message, no field value and no diff.
//! - A failure is reported with a fixed sentence and the storage error's stable kind code. The
//!   statement text, a row value and every field of the event stay out of the message.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_doc_audit::step::outbox_event_of;
use spot_doc_audit::{DenialWriter, DocAuditError, DocAuditEvent};
use spot_storage_contract::{
    Catalog, Isolation, Name, Outbox, StorageBackend, StorageError, Transaction, TxOptions,
};

use crate::error::SITE_MISMATCH;

/// The transaction label of a denial write, for tracing and metrics.
const TX_LABEL: &str = "docaudit_denial";

/// Writes denial events to the outbox of one site, each in its own transaction.
///
/// `B` is the site's storage backend (the PostgreSQL backend in production); `catalog` is the
/// catalog snapshot its transactions run against.
pub struct PgDenialWriter<B: StorageBackend> {
    site: String,
    backend: Arc<B>,
    catalog: Arc<Catalog>,
}

impl<B: StorageBackend> PgDenialWriter<B> {
    /// A writer for `site` over `backend`. Nothing is read or created.
    #[must_use]
    pub fn new(site: impl Into<String>, backend: Arc<B>, catalog: Arc<Catalog>) -> Self {
        Self {
            site: site.into(),
            backend,
            catalog,
        }
    }

    /// The site this writer writes for.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }
}

/// A failure with a fixed sentence and the stable code of the storage error; never the error's
/// own explanation (it can carry statement text or a value).
fn failed(what: &str, error: &StorageError) -> DocAuditError {
    DocAuditError::OutboxWrite(format!("{what} (storage error {})", error.kind.code()))
}

impl<B: StorageBackend + 'static> PgDenialWriter<B> {
    async fn write_one(&self, event: &DocAuditEvent) -> Result<(), DocAuditError> {
        if event.site != self.site {
            return Err(DocAuditError::OutboxWrite(format!(
                "{SITE_MISMATCH}: the denial event belongs to another site and was not written"
            )));
        }
        // Encoded first: a malformed event never opens a transaction. The encoding error is
        // replaced by a fixed sentence (it can name a value).
        let outbox_event = outbox_event_of(event).map_err(|_| {
            DocAuditError::OutboxWrite("the denial event could not be encoded".to_owned())
        })?;
        let label = Name::new(TX_LABEL).map_err(|_| {
            DocAuditError::OutboxWrite("the denial transaction label is invalid".to_owned())
        })?;
        let mut tx = self
            .backend
            .begin(
                Arc::clone(&self.catalog),
                TxOptions::new(label, Isolation::ReadCommitted),
            )
            .await
            .map_err(|e| failed("the denial transaction could not be opened", &e))?;
        if let Err(error) = tx.enqueue(&outbox_event).await {
            // Best effort: dropping the transaction would roll it back too.
            let _ = tx.rollback().await;
            return Err(failed("the denial event could not be written", &error));
        }
        tx.commit()
            .await
            .map_err(|e| failed("the denial transaction could not be committed", &e))
    }
}

impl<B: StorageBackend + 'static> DenialWriter for PgDenialWriter<B> {
    fn write<'a>(
        &'a self,
        event: &'a DocAuditEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), DocAuditError>> + Send + 'a>> {
        Box::pin(self.write_one(event))
    }
}
