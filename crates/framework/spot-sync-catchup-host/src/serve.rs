//! The hub side: serving chunks from the journal's snapshot and from a catch-up plan.
//!
//! A bootstrap chunk is a whole number of dependency groups of the requesting node's scope, cut by
//! `spot-sync-bootstrap` from the latest **verified** snapshot; a catch-up chunk is one piece of
//! the planner's output. Both are sealed with their hash ([`WireChunk`]). Nothing here decides the
//! scope: the hub host supplies it per node ([`SeamsProvider`]) from the node's *registered*
//! subscription, never from anything the node sent.
//!
//! A chunk is **served idempotently**: asking for chunk `n` again (after a cut) plans again and
//! gives the same chunk. The hub's bookkeeping moves only on [`ChunkServer::ack`], when the node
//! reports the chunk applied.

use std::sync::Arc;

use spot_sync_bootstrap::{
    Bootstrap, BootstrapError, Budget, HubHead, NodeState, ScopeView, Seams, SnapshotSource,
    StartOptions,
};
use spot_sync_catchup::CatchUpPlan;
use spot_sync_closure::{Limits, Redactor, References};

use crate::director::{ChunkAck, Director};
use crate::error::HostError;
use crate::wire::{ChunkBody, WireChunk, catch_up_chunks};

/// Why a chunk was not served, or not applied. Names, counts and codes; never a record.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    /// A prerequisite is not provided on this node.
    #[error("the catch-up serving is not ready: {0}")]
    NotReady(&'static str),
    /// No snapshot has been verified yet: the node waits rather than get an unverified base.
    #[error("no verified snapshot exists yet")]
    NoVerifiedSnapshot,
    /// The hub has no registered scope for the node.
    #[error("the node has no registered scope")]
    ScopeUnknown,
    /// The bootstrap library refused (its own code follows).
    #[error("the bootstrap refused: {0}")]
    Bootstrap(&'static str),
    /// The chunk asked for is beyond the plan.
    #[error("the chunk is beyond the plan")]
    OutOfRange,
    /// The director's bookkeeping failed.
    #[error("the catch-up bookkeeping failed ({0})")]
    Director(&'static str),
    /// A chunk body could not be written as JSON.
    #[error("the chunk could not be encoded")]
    Encode,
    /// A chunk's hash does not match its body, or the body does not decode: refused whole.
    #[error("the chunk does not match its hash")]
    Tampered,
    /// A chunk arrived out of order (the index is not the next one to apply).
    #[error("the chunk is not the next one to apply")]
    OutOfOrder,
    /// The apply path refused (its code follows); nothing of the chunk is recorded as applied.
    #[error("the chunk could not be applied: {0}")]
    ApplyFailed(&'static str),
    /// The node's checkpoint could not be read or written.
    #[error("the apply checkpoint could not be stored")]
    Checkpoint,
}

impl ServeError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotReady(_) => "sync_catchup_serve.not_ready",
            Self::NoVerifiedSnapshot => "sync_catchup_serve.no_verified_snapshot",
            Self::ScopeUnknown => "sync_catchup_serve.scope_unknown",
            Self::Bootstrap(_) => "sync_catchup_serve.bootstrap_refused",
            Self::OutOfRange => "sync_catchup_serve.out_of_range",
            Self::Director(_) => "sync_catchup_serve.bookkeeping_failed",
            Self::Encode => "sync_catchup_serve.encode_failed",
            Self::Tampered => "sync_catchup_serve.chunk_tampered",
            Self::OutOfOrder => "sync_catchup_serve.chunk_out_of_order",
            Self::ApplyFailed(_) => "sync_catchup_serve.apply_failed",
            Self::Checkpoint => "sync_catchup_serve.checkpoint_failed",
        }
    }
}

impl From<BootstrapError> for ServeError {
    fn from(error: BootstrapError) -> Self {
        Self::Bootstrap(error.code())
    }
}

impl From<HostError> for ServeError {
    fn from(error: HostError) -> Self {
        Self::Director(error.code())
    }
}

/// The latest verified snapshot of the tenant, opened for one request.
pub trait SnapshotProvider: Send + Sync {
    /// The snapshot source.
    ///
    /// # Errors
    /// [`ServeError::NoVerifiedSnapshot`] while none is verified, or the store's failure.
    fn open(&self) -> Result<Box<dyn SnapshotSource>, ServeError>;
}

/// What the hub knows of one node's scope, for one request.
pub struct NodeSeams {
    /// The node's registered scope.
    pub scope: Box<dyn ScopeView>,
    /// The link catalogue.
    pub references: Box<dyn References>,
    /// `excluded_fields.down`.
    pub redactor: Box<dyn Redactor>,
}

/// The hub host's view of a node's scope.
pub trait SeamsProvider: Send + Sync {
    /// The seams of `node`, or `None` for a node the hub has no registered scope for.
    fn for_node(&self, node: &str) -> Option<NodeSeams>;
}

/// Serves chunks. Blocking: call it from the blocking pool.
pub struct ChunkServer {
    director: Arc<Director>,
    snapshots: Arc<dyn SnapshotProvider>,
    seams: Arc<dyn SeamsProvider>,
    limits: Limits,
}

impl ChunkServer {
    /// A server over the director, the snapshot provider and the scope provider.
    #[must_use]
    pub fn new(
        director: Arc<Director>,
        snapshots: Arc<dyn SnapshotProvider>,
        seams: Arc<dyn SeamsProvider>,
        limits: Limits,
    ) -> Self {
        Self {
            director,
            snapshots,
            seams,
            limits,
        }
    }

    /// Bootstrap chunk `index` for `node`. The last chunk carries `resume_from = P + 1` and tells
    /// the director how many chunks there were.
    ///
    /// # Errors
    /// [`ServeError`].
    pub fn serve_bootstrap(
        &self,
        node: &NodeState,
        hub: HubHead,
        index: u64,
    ) -> Result<WireChunk, ServeError> {
        let source = self.snapshots.open()?;
        let seams = self
            .seams
            .for_node(&node.node_id)
            .ok_or(ServeError::ScopeUnknown)?;
        let mut session = Bootstrap::start(
            &*source,
            Seams {
                scope: &*seams.scope,
                references: &*seams.references,
                redactor: &*seams.redactor,
            },
            self.limits,
            node,
            hub,
            StartOptions::default(),
        )?;
        let budget = Budget::of(&self.limits);
        // Planning is deterministic: skipping the chunks already applied replays them.
        for _ in 0..index {
            session.next_chunk(budget)?.ok_or(ServeError::OutOfRange)?;
        }
        let chunk = session.next_chunk(budget)?.ok_or(ServeError::OutOfRange)?;
        let last = chunk.resume_from.is_some();
        let resume_from = chunk.resume_from;
        if last {
            self.director
                .plan_cut(&node.node_id, index.saturating_add(1))?;
        }
        WireChunk::seal(index, last, resume_from, &ChunkBody::Bootstrap(chunk))
    }

    /// Catch-up chunk `index` of `plan` (the hub host plans with the planner and gives the plan;
    /// the same plan gives the same chunks).
    ///
    /// # Errors
    /// [`ServeError::OutOfRange`] past the end, [`ServeError::Encode`].
    pub fn serve_catch_up(
        &self,
        node: &str,
        plan: &CatchUpPlan,
        index: u64,
    ) -> Result<WireChunk, ServeError> {
        let chunks = catch_up_chunks(plan);
        let at = usize::try_from(index).map_err(|_| ServeError::OutOfRange)?;
        let chunk = chunks.get(at).ok_or(ServeError::OutOfRange)?.clone();
        let last = at + 1 == chunks.len();
        if last {
            self.director
                .plan_cut(node, u64::try_from(chunks.len()).unwrap_or(u64::MAX))?;
        }
        WireChunk::seal(index, last, None, &ChunkBody::CatchUp(chunk))
    }

    /// The node reports chunk `index` applied. Out of order is refused; the last chunk ends the
    /// bootstrap and frees its slot.
    ///
    /// # Errors
    /// [`ServeError::Director`].
    pub fn ack(&self, node: &str, index: u64, now_ms: i64) -> Result<ChunkAck, ServeError> {
        let ack = self.director.chunk_done(node, index, now_ms)?;
        if ack == ChunkAck::Complete {
            self.director.finish(node)?;
        }
        Ok(ack)
    }
}
