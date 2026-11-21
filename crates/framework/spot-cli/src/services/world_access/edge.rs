//! The edge part of the world (WP-AI-07.T7): the real [`EdgeOps`] over an [`EdgeDesk`], the seam
//! through which the site's edge sync is read and resynchronised.
//!
//! # Why a seam, and why `from_site` finds nothing yet
//!
//! The brief maps edge ops to the sync services (position and lag per edge, a catch-up from the
//! hub). What the repository has today is different: `services/sync_node`, `sync_hub` and
//! `sync_catchup*` expose no public port that lists an edge's position or asks for a catch-up (the
//! hub's `HubParts` and the node's loops are private wiring), and `services/edge_integrity` is the
//! document-graph drift scan, whose repair is a report-keyed batch with its own audit rows (no
//! per-edge repair, no undo). Nothing fits `EdgeOps` without inventing an operation in a file this
//! task does not own. So the part is written over a small seam, [`EdgeDesk`], that a later task
//! implements over whichever service gets the port, and `from_site` answers `None` (the readiness
//! text says `no edge`). See the report's open questions for the operations to add.
//!
//! # What the part does over a desk
//!
//! - `drifted` lists the edges whose actual position differs from the expected one; `edge` reads
//!   one (a desk that cannot answer reads as "none").
//! - `repair` asks the desk to resynchronise the edge and reports the positions before and after.
//! - `restore` is **not supported by the edge port** (a resync cannot be un-applied), except to the
//!   position the edge already has.
//! - `rehearsal` copies the desk's edges into the in-memory fixture; a rehearsal repair never
//!   reaches the desk.
//!
//! The ops are synchronous and block on the runtime handle captured when the site was built
//! (`Handle::block_on`, never `block_in_place`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_remedies::fixture::FakeEdge;
use spot_remedies::{EdgeChange, EdgeOps, EdgeRef, EdgeView, OpError};
use tokio::runtime::Handle;

use super::parts::{EdgePart, PartName};
use crate::services::SiteCtx;
use crate::services::remedies::world::WorldError;

const NOT_SUPPORTED: &str = "not supported by the edge port";

/// The state of one edge: where it should be and where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeState {
    /// The edge's stable name.
    pub edge: String,
    /// The position (a count) the hub says it should be at.
    pub expected: u64,
    /// The position it is at.
    pub actual: u64,
}

/// A desk call: the answer, or a fixed reason it could not be given (no data in it).
pub type DeskFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;

/// The seam over the edge sync.
pub trait EdgeDesk: Send + Sync {
    /// Every known edge with its expected and actual position.
    fn edges(&self) -> DeskFuture<'_, Vec<EdgeState>>;

    /// Resynchronises `edge` (a catch-up from its position) and answers its state afterwards.
    fn resync<'a>(&'a self, edge: &'a str) -> DeskFuture<'a, EdgeState>;
}

/// The edge part over a desk.
pub struct DeskEdgePart {
    desk: Arc<dyn EdgeDesk>,
    handle: Handle,
}

impl DeskEdgePart {
    /// A part over `desk`; `handle` is the runtime the ops block on.
    #[must_use]
    pub fn new(desk: Arc<dyn EdgeDesk>, handle: Handle) -> Self {
        Self { desk, handle }
    }

    fn edges(&self) -> Result<Vec<EdgeState>, &'static str> {
        self.handle.block_on(self.desk.edges())
    }
}

/// The part for a desk, or `None` when there is no desk or no runtime to block on.
#[must_use]
pub fn from_parts(
    desk: Option<Arc<dyn EdgeDesk>>,
    handle: Option<Handle>,
) -> Option<Arc<dyn EdgePart>> {
    Some(Arc::new(DeskEdgePart::new(desk?, handle?)))
}

/// The edge part for this site: `None` until a service provides an [`EdgeDesk`] (none does yet).
pub(super) fn from_site(ctx: &SiteCtx<'_>) -> Option<Arc<dyn EdgePart>> {
    from_parts(ctx.port::<dyn EdgeDesk>(), super::parts::capture_runtime())
}

impl EdgePart for DeskEdgePart {
    fn live(&self) -> Box<dyn EdgeOps + '_> {
        Box::new(DeskEdgeOps { part: self })
    }

    fn rehearsal(&self) -> Result<Box<dyn EdgeOps>, WorldError> {
        let edges = self
            .edges()
            .map_err(|_| PartName::Edge.rehearsal_unavailable())?;
        let mut fake = FakeEdge::new();
        for state in edges {
            fake.seed(&state.edge, state.expected, state.actual);
        }
        Ok(Box::new(fake))
    }
}

/// The live ops.
struct DeskEdgeOps<'a> {
    part: &'a DeskEdgePart,
}

fn view_of(state: &EdgeState) -> EdgeView {
    EdgeView {
        edge: EdgeRef::new(&state.edge),
        expected: state.expected,
        actual: state.actual,
    }
}

impl EdgeOps for DeskEdgeOps<'_> {
    fn drifted(&self) -> Vec<EdgeRef> {
        self.part
            .edges()
            .map(|edges| {
                edges
                    .iter()
                    .filter(|state| state.expected != state.actual)
                    .map(|state| EdgeRef::new(&state.edge))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn edge(&self, edge: &EdgeRef) -> Option<EdgeView> {
        self.part
            .edges()
            .ok()?
            .iter()
            .find(|state| state.edge == edge.as_str())
            .map(view_of)
    }

    fn repair(&mut self, edge: &EdgeRef) -> Result<EdgeChange, OpError> {
        let op = "edge_repair";
        let before = self
            .edge(edge)
            .ok_or_else(|| OpError::new(op, &format!("no edge `{}`", edge.as_str())))?;
        let after = self
            .part
            .handle
            .block_on(self.part.desk.resync(edge.as_str()))
            .map_err(|reason| OpError::new(op, reason))?;
        Ok(EdgeChange {
            edge: edge.clone(),
            before: before.actual,
            after: after.actual,
        })
    }

    fn restore(&mut self, change: &EdgeChange) -> Result<(), OpError> {
        let op = "edge_restore";
        let now = self
            .edge(&change.edge)
            .ok_or_else(|| OpError::new(op, &format!("no edge `{}`", change.edge.as_str())))?;
        if now.actual == change.before {
            return Ok(());
        }
        Err(OpError::new(op, NOT_SUPPORTED))
    }
}
