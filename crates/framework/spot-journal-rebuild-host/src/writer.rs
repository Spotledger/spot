//! The PostgreSQL apply sink: stored versions through the one replication apply path.
//!
//! [`ReplicaSink`] is the [`ApplySink`] that writes the rebuilt database. Each image becomes a
//! [`ReceivedRecord`] and goes through `spot_replica_apply::apply`: shape-validated against the
//! catalogue, idempotent by name and version, written **without** controllers, scripts, hooks or
//! numbering, and **without enqueuing an outbox event**. A tombstone deletes the document at
//! the version the stream records. The apply path commits per record; the host checkpoints every
//! N of them ([`crate::CheckpointingSink`]).
//!
//! # The image's wire form
//!
//! ADR-061 s3 stores the whole document in a `doc_state` event; [`WireCodec`] reads that image as
//! the apply path's own wire payload (`RecordPayload`). A deployment whose images differ supplies
//! its own [`ImageCodec`].

use std::sync::Arc;

use spot_events_contract::SourceId;
use spot_journal_rebuild::{
    AppliedAudit, AppliedDelete, AppliedImage, ApplySink, PlanSummary, SinkError, SinkFuture,
};
use spot_postgres::PgBackend;
use spot_replica_apply::{
    ApplyCtx, ApplyOutcome, OwnAll, ReceivedRecord, RecordingAudit, SideEffects, apply,
};
use spot_storage_contract::{
    Catalog, DocumentStore, Isolation, Name, RecordId, StorageBackend, Transaction, TxOptions,
    Version,
};
use spot_sync_protocol::RecordName;

/// Turns one image into the apply path's record.
pub trait ImageCodec: Send + Sync {
    /// The record for `image`, from `source`.
    ///
    /// # Errors
    /// A sentence without content when the image cannot be read as a record.
    fn record(&self, image: &AppliedImage, source: &SourceId) -> Result<ReceivedRecord, String>;
}

/// Reads the image as `RecordPayload` JSON (see the module docs).
pub struct WireCodec;

impl ImageCodec for WireCodec {
    fn record(&self, image: &AppliedImage, source: &SourceId) -> Result<ReceivedRecord, String> {
        let name = RecordName::new(&format!("{}/{}", image.entity, image.id))
            .map_err(|_| "the record name is not valid".to_owned())?;
        let payload = serde_json::Value::Object(image.image.body().clone()).to_string();
        ReceivedRecord::from_payload(source, name, image.version, image.position.seq, &payload)
            .map_err(|reason| format!("the image is not a record ({})", reason.code()))
    }
}

/// A panicking sentinel proving the apply path runs no business logic: if anything in the path
/// reached a side effect, the rebuild would stop. Production passes none; tests pass this.
pub struct NoSideEffects;

impl SideEffects for NoSideEffects {
    fn run_hooks(&self, entity: &str) {
        panic!("the rebuild's apply path ran a hook for {entity}");
    }
    fn allocate_name(&self, entity: &str) -> String {
        panic!("the rebuild's apply path allocated a name for {entity}");
    }
    fn run_script(&self, entity: &str) {
        panic!("the rebuild's apply path ran a script for {entity}");
    }
    fn emit_event(&self, entity: &str) {
        panic!("the rebuild's apply path emitted an event for {entity}");
    }
}

/// The sink over a PostgreSQL backend.
pub struct ReplicaSink {
    backend: PgBackend,
    catalog: Arc<Catalog>,
    source: SourceId,
    codec: Arc<dyn ImageCodec>,
    at_micros: i64,
    audit: RecordingAudit,
    sentinel: Option<Arc<dyn SideEffects>>,
}

impl ReplicaSink {
    /// A sink writing `catalog`'s tables through `backend`. `at_micros` is the instant the apply
    /// path stamps (supplied by the caller; the library reads no clock).
    #[must_use]
    pub fn new(
        backend: PgBackend,
        catalog: Arc<Catalog>,
        source: SourceId,
        codec: Arc<dyn ImageCodec>,
        at_micros: i64,
    ) -> Self {
        Self {
            backend,
            catalog,
            source,
            codec,
            at_micros,
            audit: RecordingAudit::new(),
            sentinel: None,
        }
    }

    /// Passes a sentinel to the apply path that stops the rebuild if any side effect is reached
    /// (tests pass [`NoSideEffects`]; production passes none).
    #[must_use]
    pub fn with_sentinel(mut self, sentinel: Arc<dyn SideEffects>) -> Self {
        self.sentinel = Some(sentinel);
        self
    }

    async fn begin_tx(&self, label: &str) -> Result<spot_postgres::PgTransaction, SinkError> {
        let name = Name::new(label).map_err(|_| SinkError::new("journal_rebuild_host.label"))?;
        self.backend
            .begin(
                Arc::clone(&self.catalog),
                TxOptions::new(name, Isolation::ReadCommitted),
            )
            .await
            .map_err(|_| SinkError::new("journal_rebuild_host.db_unavailable"))
    }
}

impl ApplySink for ReplicaSink {
    fn begin<'a>(&'a mut self, _summary: &'a PlanSummary) -> SinkFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn apply_image(&mut self, image: AppliedImage) -> SinkFuture<'_> {
        Box::pin(async move {
            let record = self
                .codec
                .record(&image, &self.source)
                .map_err(SinkError::new)?;
            let tx = self.begin_tx("journal_rebuild_apply").await?;
            let owner = OwnAll;
            let mut ctx = ApplyCtx::new(&owner, &self.audit, self.at_micros);
            if let Some(sentinel) = &self.sentinel {
                ctx = ctx.with_side_effects(sentinel.as_ref());
            }
            match apply(tx, &record, &ctx).await {
                ApplyOutcome::Stored | ApplyOutcome::Duplicate | ApplyOutcome::Stale => Ok(()),
                other => Err(SinkError::new(
                    other
                        .reason_code()
                        .unwrap_or("journal_rebuild_host.apply_refused"),
                )),
            }
        })
    }

    fn apply_delete(&mut self, delete: AppliedDelete) -> SinkFuture<'_> {
        Box::pin(async move {
            let entity = Name::new(&delete.entity)
                .map_err(|_| SinkError::new("journal_rebuild_host.entity"))?;
            let id = parse_id(&delete.id)?;
            let mut tx = self.begin_tx("journal_rebuild_delete").await?;
            match tx.delete(&entity, id, Version(delete.version)).await {
                Ok(()) => tx
                    .commit()
                    .await
                    .map_err(|_| SinkError::new("journal_rebuild_host.db_unavailable")),
                Err(_) => {
                    // Already gone (a resume re-applies a delete): the rebuild's goal holds.
                    let _ = tx.rollback().await;
                    Ok(())
                }
            }
        })
    }

    fn apply_audit(&mut self, _audit: AppliedAudit) -> SinkFuture<'_> {
        // Audit-class entries are part of the recoverable record in the journal; the rebuilt
        // database's own audit chain is not rewritten from them (it would break its hash chain).
        Box::pin(std::future::ready(Ok(())))
    }

    fn commit(
        &mut self,
        _last_position: spot_events_contract::envelope::Position,
    ) -> SinkFuture<'_> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn abort<'a>(&'a mut self, _reason: &'a str) -> SinkFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }
}

fn parse_id(text: &str) -> Result<RecordId, SinkError> {
    let uuid =
        uuid::Uuid::parse_str(text).map_err(|_| SinkError::new("journal_rebuild_host.id"))?;
    RecordId::from_bytes(*uuid.as_bytes()).map_err(|_| SinkError::new("journal_rebuild_host.id"))
}
