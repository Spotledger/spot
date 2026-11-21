//! The projector: committed outbox records in, per-record results out.
//!
//! [`Projector::process`] is the same shape as the webhook intake (WP-EXT-08): one committed
//! [`OutboxRecord`] in, one [`ProjectionReport`] out. It decodes the record into an edge event,
//! projects the event's properties through the [`ProjectionConfig`] (dropping sensitive fields),
//! applies it to the [`GraphSink`], and only then advances the durable checkpoint. So:
//!
//! - **Idempotent.** A record at or below the checkpoint is [`Projected::Skipped`]; a record
//!   redelivered after a crash but before the checkpoint was saved is ignored by the sink (its
//!   sequence is already applied).
//! - **Resumable.** A new projector over the same sink and checkpoint store continues where the
//!   last one stopped.
//! - **Ordered per source record.** Records are processed in the order given, and a record's
//!   events in the order the sink receives them.
//! - **Non-blocking.** A malformed edge payload is [`Projected::Rejected`] and the checkpoint
//!   still advances, so one poison record cannot stall the feed forever.
//!
//! The projector never reads the wall clock (a report carries no time) and never writes storage.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_graph_index::GraphSource;
use spot_storage_contract::{FeedPosition, OutboxRecord};

use crate::checkpoint::CheckpointStore;
use crate::config::{FieldSensitivity, ProjectionConfig};
use crate::decode::{EdgeTopic, decode_record_with_topic};
use crate::error::ProjectionError;
use crate::sink::{EdgeBatch, GraphSink, SinkOutcome};

/// What one call to [`Projector::process`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projected {
    /// The record was already processed (at or below the checkpoint): nothing changed.
    Skipped,
    /// The record is not an edge event: it was passed over, and the checkpoint advanced.
    NotEdge,
    /// The record's edge event was applied to the sink.
    Applied(SinkOutcome),
    /// The record claimed to be an edge event but its payload was malformed; it was reported and
    /// the checkpoint advanced.
    Rejected {
        /// The error code, for the operator.
        code: String,
        /// Why.
        reason: String,
    },
}

/// What processing one record did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionReport {
    /// The record's outbox position.
    pub position: FeedPosition,
    /// The sequence the record carried (its feed position's sequence).
    pub sequence: u64,
    /// Which name of the edge topic the record carried; `None` when it is not an edge record or
    /// was skipped before decoding. [`EdgeTopic::RetiredAlias`] means a `doc_edge` record.
    pub edge_topic: Option<EdgeTopic>,
    /// What happened.
    pub projected: Projected,
}

/// What a bulk rebuild did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildReport {
    /// How many edges the snapshot streamed.
    pub edges: usize,
    /// The source's watermark the sink now carries.
    pub watermark: u64,
}

/// Projects committed outbox records into an external graph sink.
pub struct Projector {
    sink: Arc<dyn GraphSink>,
    checkpoints: Arc<dyn CheckpointStore>,
    config: ProjectionConfig,
    sensitivity: Arc<dyn FieldSensitivity>,
    retired_alias_reads: AtomicU64,
}

impl Projector {
    /// A projector over `sink` and `checkpoints` for `config`, with `sensitivity` guarding the
    /// fields the config allows.
    #[must_use]
    pub fn new(
        sink: Arc<dyn GraphSink>,
        checkpoints: Arc<dyn CheckpointStore>,
        config: ProjectionConfig,
        sensitivity: Arc<dyn FieldSensitivity>,
    ) -> Self {
        Self {
            sink,
            checkpoints,
            config,
            sensitivity,
            retired_alias_reads: AtomicU64::new(0),
        }
    }

    /// How many records this projector decoded under the retired `doc_edge` name. When it stays
    /// zero for the whole retention horizon the alias is dead and can be removed.
    #[must_use]
    pub fn retired_alias_reads(&self) -> u64 {
        self.retired_alias_reads.load(Ordering::Relaxed)
    }

    /// The config this projector feeds.
    #[must_use]
    pub fn config(&self) -> &ProjectionConfig {
        &self.config
    }

    /// Processes one committed record.
    ///
    /// # Errors
    ///
    /// [`ProjectionError`] for an infrastructure failure (the sink or the checkpoint store), or
    /// [`ProjectionError::Resync`] when the record's outbox generation differs from the stored
    /// checkpoint's. A malformed payload is a [`Projected::Rejected`] report, never an error.
    pub fn process(&self, record: &OutboxRecord) -> Result<ProjectionReport, ProjectionError> {
        let key = self.config.graph().clone();
        let position = record.position;
        if let Some(stored) = self.checkpoints.load(&key)? {
            if stored != FeedPosition::START && stored.generation() != position.generation() {
                return Err(ProjectionError::Resync {
                    generation: position.generation(),
                });
            }
            if position <= stored {
                return Ok(ProjectionReport {
                    position,
                    sequence: position.seq(),
                    edge_topic: None,
                    projected: Projected::Skipped,
                });
            }
        }

        let (event, edge_topic) = match decode_record_with_topic(record) {
            Ok(Some(decoded)) => decoded,
            Ok(None) => {
                self.checkpoints.save(&key, position)?;
                return Ok(ProjectionReport {
                    position,
                    sequence: position.seq(),
                    edge_topic: None,
                    projected: Projected::NotEdge,
                });
            }
            Err(error) => {
                self.checkpoints.save(&key, position)?;
                return Ok(ProjectionReport {
                    position,
                    sequence: position.seq(),
                    edge_topic: EdgeTopic::of(record.event.topic.as_str()),
                    projected: Projected::Rejected {
                        code: error.code().to_owned(),
                        reason: error.to_string(),
                    },
                });
            }
        };

        if edge_topic == EdgeTopic::RetiredAlias {
            self.retired_alias_reads.fetch_add(1, Ordering::Relaxed);
        }
        let event = self.config.project(&event, self.sensitivity.as_ref());
        let batch = EdgeBatch::events(key.clone(), vec![event]);
        let outcome = self.sink.apply(&batch)?;
        self.checkpoints.save(&key, position)?;
        Ok(ProjectionReport {
            position,
            sequence: position.seq(),
            edge_topic: Some(edge_topic),
            projected: Projected::Applied(outcome),
        })
    }

    /// Processes a batch of committed records, in order, stopping at the first infrastructure
    /// failure. A rejected record does not stop the batch.
    ///
    /// # Errors
    ///
    /// As [`Projector::process`].
    pub fn process_batch(
        &self,
        records: &[OutboxRecord],
    ) -> Result<Vec<ProjectionReport>, ProjectionError> {
        let mut reports = Vec::with_capacity(records.len());
        for record in records {
            reports.push(self.process(record)?);
        }
        Ok(reports)
    }

    /// Rebuilds the projection from a full snapshot: resets the sink, streams the source's edges
    /// as one snapshot batch, and clears the durable checkpoint.
    ///
    /// Clearing the checkpoint means the outbox feed is read again from its start; the sink
    /// ignores every event at or below the snapshot's watermark, so the rebuild is idempotent.
    ///
    /// # Errors
    ///
    /// [`ProjectionError`] when the sink, the source or the checkpoint store fails.
    pub fn rebuild(&self, source: &dyn GraphSource) -> Result<RebuildReport, ProjectionError> {
        let key = self.config.graph().clone();
        self.sink.reset()?;
        let edges = source.stream_edges(&key).map_err(ProjectionError::from)?;
        let watermark = source.watermark(&key).map_err(ProjectionError::from)?;
        let batch = EdgeBatch::snapshot(key.clone(), watermark, edges.clone());
        self.sink.apply(&batch)?;
        self.checkpoints.clear(&key)?;
        Ok(RebuildReport {
            edges: edges.len(),
            watermark,
        })
    }
}
