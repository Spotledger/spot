//! The checkpointing sink: resume, counting and the durable checkpoint around any apply sink.
//!
//! [`CheckpointingSink`] wraps the sink that writes the database. It
//! * skips what the saved checkpoint covers, so a resumed rebuild neither restarts from zero nor
//!   applies anything twice (the executor re-reads the base and the segments; the skip is by
//!   position, and the apply path under it is idempotent by name and version as well);
//! * saves the checkpoint `(generation, seq)` every `every` applied items and at the end, and
//!   keeps the state `Rebuilding` until `commit`;
//! * counts what it applied and skipped, for the report (counts only).
//!
//! One transaction **per image** is the apply path's rule (`spot_replica_apply::apply` commits
//! what it applies); the batch is therefore the checkpoint interval, not a transaction.

use std::sync::Arc;

use spot_events_contract::envelope::Position;
use spot_journal_rebuild::{
    AppliedAudit, AppliedDelete, AppliedImage, ApplySink, PlanSummary, SinkError, SinkFuture,
};

use crate::checkpoint::{Checkpoint, CheckpointStore, State};

/// What the sink did, for the report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SinkCounts {
    /// Images applied by this run.
    pub images: u64,
    /// Deletes applied by this run.
    pub deletes: u64,
    /// Audit entries handed over by this run.
    pub audits: u64,
    /// Items skipped because the checkpoint covers them.
    pub skipped_by_resume: u64,
}

/// The wrapper; see the module docs.
pub struct CheckpointingSink {
    inner: Box<dyn ApplySink>,
    store: Arc<dyn CheckpointStore>,
    site: String,
    plan_hash: String,
    every: u64,
    resume_after: Option<Position>,
    since_save: u64,
    /// The position of the item being applied (several snapshot images share one position).
    pending: Option<Position>,
    /// The latest position **every** item of which has been applied: what a checkpoint may say.
    completed: Option<Position>,
    counts: SinkCounts,
}

/// Whether `a` is at or before `b` in the stream's order.
fn covered(a: Position, b: Position) -> bool {
    (a.generation, a.seq) <= (b.generation, b.seq)
}

impl CheckpointingSink {
    /// Wraps `inner`. `resume_after` is the saved checkpoint's position when resuming.
    pub fn new(
        inner: Box<dyn ApplySink>,
        store: Arc<dyn CheckpointStore>,
        site: &str,
        plan_hash: &str,
        every: u64,
        resume_after: Option<Position>,
    ) -> Self {
        Self {
            inner,
            store,
            site: site.to_owned(),
            plan_hash: plan_hash.to_owned(),
            every: every.max(1),
            resume_after,
            since_save: 0,
            pending: None,
            completed: resume_after,
            counts: SinkCounts::default(),
        }
    }

    /// What was done so far.
    pub fn counts(&self) -> SinkCounts {
        self.counts
    }

    /// The wrapped sink.
    pub fn inner(&self) -> &dyn ApplySink {
        self.inner.as_ref()
    }

    fn skip(&mut self, position: Position) -> bool {
        match self.resume_after {
            Some(done) if covered(position, done) => {
                self.counts.skipped_by_resume += 1;
                true
            }
            _ => false,
        }
    }

    /// Notes that an item at `position` is about to be applied: when the stream has moved past
    /// the previous item's position, everything at that position is complete.
    fn moving_to(&mut self, position: Position) {
        if let Some(previous) = self.pending
            && !covered(position, previous)
        {
            self.completed = Some(previous);
        }
        self.pending = Some(position);
    }

    async fn after(&mut self) -> Result<(), SinkError> {
        self.since_save += 1;
        if self.since_save >= self.every && self.completed.is_some() {
            let at = self.completed;
            self.save(at, State::Rebuilding).await?;
        }
        Ok(())
    }

    async fn save(&mut self, at: Option<Position>, state: State) -> Result<(), SinkError> {
        let Some(at) = at else {
            return Ok(());
        };
        self.since_save = 0;
        let checkpoint = Checkpoint {
            plan_hash: self.plan_hash.clone(),
            generation: at.generation,
            seq: at.seq,
            state,
        };
        self.store
            .save(&self.site, &checkpoint)
            .await
            .map_err(|e| SinkError::new(e.code()))
    }
}

impl ApplySink for CheckpointingSink {
    fn begin<'a>(&'a mut self, summary: &'a PlanSummary) -> SinkFuture<'a> {
        Box::pin(async move {
            // Mark the database as being rebuilt before the first write.
            let at = self.resume_after.unwrap_or(Position {
                generation: 0,
                seq: 0,
            });
            let checkpoint = Checkpoint {
                plan_hash: self.plan_hash.clone(),
                generation: at.generation,
                seq: at.seq,
                state: State::Rebuilding,
            };
            self.store
                .save(&self.site, &checkpoint)
                .await
                .map_err(|e| SinkError::new(e.code()))?;
            self.inner.begin(summary).await
        })
    }

    fn apply_image(&mut self, image: AppliedImage) -> SinkFuture<'_> {
        Box::pin(async move {
            let position = image.position;
            if self.skip(position) {
                return Ok(());
            }
            self.moving_to(position);
            self.inner.apply_image(image).await?;
            self.counts.images += 1;
            self.after().await
        })
    }

    fn apply_delete(&mut self, delete: AppliedDelete) -> SinkFuture<'_> {
        Box::pin(async move {
            let position = delete.position;
            if self.skip(position) {
                return Ok(());
            }
            self.moving_to(position);
            self.inner.apply_delete(delete).await?;
            self.counts.deletes += 1;
            self.after().await
        })
    }

    fn apply_audit(&mut self, audit: AppliedAudit) -> SinkFuture<'_> {
        Box::pin(async move {
            let position = audit.position;
            if self.skip(position) {
                return Ok(());
            }
            self.moving_to(position);
            self.inner.apply_audit(audit).await?;
            self.counts.audits += 1;
            self.after().await
        })
    }

    fn commit(&mut self, last_position: Position) -> SinkFuture<'_> {
        Box::pin(async move {
            self.inner.commit(last_position).await?;
            // The state stays `Rebuilding` until the host has adopted the outbox position and
            // run the projectors and invariants: only then does it mark the rebuild complete.
            self.save(Some(last_position), State::Rebuilding).await
        })
    }

    fn abort<'a>(&'a mut self, reason: &'a str) -> SinkFuture<'a> {
        Box::pin(async move {
            // The checkpoint stays (state Rebuilding): an interrupted rebuild resumes from it.
            self.inner.abort(reason).await
        })
    }
}
