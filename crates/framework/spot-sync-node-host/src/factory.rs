//! The seam the host fills: how the receiving side of one tenant is built.
//!
//! The apply path of an edge needs the site's ownership view, its audit, its quarantine, its
//! metadata catalogue and its clock (`spot_sync_node_pg::ApplyEnv`). Those belong to the host's
//! modules, not to this crate: a module that has them provides an [`IntakeFactory`] port, and the
//! `sync_node` service builds the intake through it **on the worker's plain thread** (the
//! PostgreSQL stores refuse to be built inside an async runtime).

use std::sync::Arc;

use spot_events_contract::SourceId;
use spot_pg_bridge::Bridge;

use crate::applier::Intake;
use crate::error::Result;
use crate::pg::PgCursors;

/// What the factory is given.
pub struct IntakeRequest {
    /// The shared blocking bridge (build stores with it).
    pub bridge: Arc<Bridge>,
    /// The size of the pool the bridge was built on.
    pub pool_size: usize,
    /// The tenant (the site's label).
    pub tenant: String,
    /// The hub's stream: the only source a delivery may come from.
    pub hub_source: SourceId,
    /// The cursor stores (`serve`, `push`) of this tenant.
    pub cursors: PgCursors,
}

/// Builds the intake of one tenant.
pub trait IntakeFactory: Send + Sync + 'static {
    /// The metadata epoch the node's records are written under (the hello's epoch).
    fn metadata_epoch(&self) -> u64;

    /// Builds the intake. Called once per service start, on a plain thread.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed` (or the store's own failure, mapped) when a table is missing
    /// or the apply path cannot be built.
    fn build(&self, request: IntakeRequest) -> Result<Box<dyn Intake>>;
}
