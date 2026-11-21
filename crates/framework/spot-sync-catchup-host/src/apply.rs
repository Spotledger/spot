//! The node side: verify, apply and resume.
//!
//! [`ChunkApplier::apply`] checks the chunk's hash, refuses an index that is not the next one,
//! hands the body to the node's apply path ([`ApplyTarget`], one transaction per bundle, idempotent
//! by record name and version) and **only then** records the chunk as applied. A cut anywhere
//! therefore repeats at most the chunk in flight, and a repeat changes nothing. The last chunk of
//! a bootstrap gives the position to continue from on the live stream (`P + 1`).

use std::sync::Arc;

use spot_sync_bootstrap::Bundle;
use spot_sync_catchup::Event;

use crate::director::{Phase, Progress, ProgressStore};
use crate::serve::ServeError;
use crate::wire::{CatchUpChunk, ChunkBody, WireChunk};

/// The node's apply path (the one shared by sync, rebuild and links).
pub trait ApplyTarget: Send + Sync {
    /// Applies one bundle of record images in one transaction.
    ///
    /// # Errors
    /// [`ServeError::ApplyFailed`] with the apply path's code; nothing of the bundle stays.
    fn apply_bundle(&self, bundle: &Bundle) -> Result<(), ServeError>;

    /// Applies events (a mandatory event, a coalesced bundle's events, audit events) in position
    /// order.
    ///
    /// # Errors
    /// As [`ApplyTarget::apply_bundle`].
    fn apply_events(&self, events: &[Event]) -> Result<(), ServeError>;
}

/// What applying one chunk did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Applied; ask for the next chunk.
    Next,
    /// Already applied before (a repeat after a cut): nothing changed.
    Duplicate,
    /// The last chunk: the plan is complete. A bootstrap continues on the live stream from
    /// `resume_from`.
    Complete {
        /// `P + 1` for a bootstrap.
        resume_from: Option<u64>,
    },
}

/// Applies chunks from one hub.
pub struct ChunkApplier {
    hub: String,
    store: Arc<dyn ProgressStore>,
    target: Arc<dyn ApplyTarget>,
}

impl ChunkApplier {
    /// An applier for chunks of `hub`, checkpointed in `store`.
    #[must_use]
    pub fn new(hub: &str, store: Arc<dyn ProgressStore>, target: Arc<dyn ApplyTarget>) -> Self {
        Self {
            hub: hub.to_owned(),
            store,
            target,
        }
    }

    /// The chunk the node asks for next (0 for a fresh start, the checkpoint after a cut).
    ///
    /// # Errors
    /// [`ServeError::Checkpoint`].
    pub fn next_index(&self) -> Result<u64, ServeError> {
        Ok(self
            .store
            .load(&self.hub)
            .map_err(|_| ServeError::Checkpoint)?
            .map_or(0, |progress| progress.done_chunks))
    }

    /// Verifies and applies `wire`.
    ///
    /// # Errors
    /// [`ServeError::Tampered`] (nothing applied), [`ServeError::OutOfOrder`],
    /// [`ServeError::ApplyFailed`] (the checkpoint does not move), [`ServeError::Checkpoint`].
    pub fn apply(&self, wire: &WireChunk) -> Result<Applied, ServeError> {
        let body = wire.open()?;
        let done = self.next_index()?;
        if wire.index < done {
            return Ok(Applied::Duplicate);
        }
        if wire.index > done {
            return Err(ServeError::OutOfOrder);
        }
        match &body {
            ChunkBody::Bootstrap(chunk) => {
                for bundle in &chunk.bundles {
                    self.target.apply_bundle(bundle)?;
                }
            }
            ChunkBody::CatchUp(CatchUpChunk::Mandatory(event)) => {
                self.target.apply_events(std::slice::from_ref(event))?;
            }
            ChunkBody::CatchUp(CatchUpChunk::Bulk(bundle)) => {
                self.target.apply_events(&bundle.events)?;
            }
            ChunkBody::CatchUp(CatchUpChunk::Audit(events)) => {
                self.target.apply_events(events)?;
            }
        }
        if wire.last {
            self.store
                .clear(&self.hub)
                .map_err(|_| ServeError::Checkpoint)?;
            return Ok(Applied::Complete {
                resume_from: wire.resume_from,
            });
        }
        let phase = match body {
            ChunkBody::Bootstrap(_) => Phase::Bootstrap,
            ChunkBody::CatchUp(_) => Phase::CatchUp,
        };
        self.store
            .save(
                &self.hub,
                &Progress {
                    phase,
                    total_chunks: 0,
                    done_chunks: wire.index.saturating_add(1),
                },
            )
            .map_err(|_| ServeError::Checkpoint)?;
        Ok(Applied::Next)
    }
}
