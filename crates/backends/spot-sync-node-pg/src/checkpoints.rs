//! [`PgCheckpoints`]: the protocol's `CheckpointStore` over `spot_sync_node_checkpoint`.
//!
//! # What a checkpoint is here, and why it cannot be ahead
//!
//! The stored number is the **applied journal prefix** of one source: every record up to it has
//! been applied. Two rules make "the checkpoint advances only after the apply is durable" hold
//! by construction, not by discipline:
//!
//! 1. The apply path ([`crate::PgBundleApply::apply_bundle_checkpointed`]) writes the ledger rows
//!    and the checkpoint **in the same transaction** as the documents. There is no moment at
//!    which one exists and the other does not; a failpoint test kills the transaction before the
//!    commit and asserts that neither the documents, nor the ledger, nor the checkpoint exist.
//! 2. A checkpoint stored on its own (`CheckpointStore::store`) is refused when it is above the
//!    highest position the ledger holds for the source (`sync_node_pg.checkpoint_ahead`), and
//!    never moves backward.

use std::time::Duration;

use spot_sync_protocol::{Checkpoint, CheckpointStore, Error, Result as ProtocolResult, SourceId};

use crate::db::{Db, valid_tenant};
use crate::error::{FailureLog, OpError, PgNodeError, map, to_i64, to_u64};
use crate::ledger::MAX_POSITION;

/// Deadline of a checkpoint call.
pub const CHECKPOINT_DEADLINE: Duration = Duration::from_secs(15);

pub(crate) const LOAD: &str = "SELECT last_applied_seq FROM spot_sync_node_checkpoint \
    WHERE tenant = $1 AND source = $2";
/// Forward only: a smaller value never replaces a larger one.
pub(crate) const UPSERT: &str = "INSERT INTO spot_sync_node_checkpoint (tenant, source, last_applied_seq) \
    VALUES ($1, $2, $3) ON CONFLICT (tenant, source) DO UPDATE SET \
    last_applied_seq = GREATEST(spot_sync_node_checkpoint.last_applied_seq, EXCLUDED.last_applied_seq), \
    updated_at = now()";

/// The checkpoints of one tenant.
#[derive(Clone, Debug)]
pub struct PgCheckpoints {
    db: Db,
    tenant: String,
    failures: FailureLog,
}

impl PgCheckpoints {
    /// Checkpoints of `tenant` on `db`.
    ///
    /// # Errors
    /// `sync_node_pg.unsupported_context` for an empty, long or unprintable tenant.
    pub fn new(db: Db, tenant: impl Into<String>) -> Result<Self, PgNodeError> {
        let tenant = tenant.into();
        if !valid_tenant(&tenant) {
            return Err(PgNodeError::UnsupportedContext {
                reason: "the tenant is empty, too long or not printable",
            });
        }
        Ok(Self {
            db,
            tenant,
            failures: FailureLog::new(),
        })
    }

    /// The last failure of `load` since the previous call (a failed `load` answers `None`, so the
    /// stream is re-fetched from further back: the safe direction).
    #[must_use]
    pub fn take_failure(&self) -> Option<PgNodeError> {
        self.failures.take()
    }

    fn read(&self, source: &SourceId) -> Result<Option<Checkpoint>, PgNodeError> {
        let (tenant, text) = (self.tenant.clone(), source.as_str().to_owned());
        let seq = self
            .db
            .run(CHECKPOINT_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn.query_opt(LOAD, &[&tenant, &text]).await?;
                    row.map(|r| to_u64(r.get::<_, i64>(0))).transpose()
                })
            })
            .map_err(map)?;
        Ok(seq.map(|seq| Checkpoint::new(source.clone(), seq)))
    }

    fn write(&self, checkpoint: &Checkpoint) -> Result<(), PgNodeError> {
        let seq = to_i64(checkpoint.last_applied_seq).map_err(|_| PgNodeError::StateInvalid)?;
        let (tenant, text) = (self.tenant.clone(), checkpoint.source.as_str().to_owned());
        self.db
            .run(CHECKPOINT_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = conn.transaction().await?;
                    let ahead = tx.query_one(MAX_POSITION, &[&tenant, &text]).await?;
                    let highest = ahead.get::<_, Option<i64>>(0).unwrap_or(0);
                    if seq > highest {
                        return Err(OpError::Refused(PgNodeError::CheckpointAhead));
                    }
                    tx.execute(UPSERT, &[&tenant, &text, &seq]).await?;
                    tx.commit().await?;
                    Ok(())
                })
            })
            .map_err(map)
    }
}

impl CheckpointStore for PgCheckpoints {
    fn load(&self, source: &SourceId) -> Option<Checkpoint> {
        match self.read(source) {
            Ok(found) => found,
            Err(error) => {
                self.failures.record(error);
                None
            }
        }
    }

    fn store(&mut self, checkpoint: &Checkpoint) -> ProtocolResult<()> {
        self.write(checkpoint)
            .map_err(|error| Error::store_failed(error.to_string()))
    }
}
