//! Typed errors with stable `sync_catchup_host.*` codes.

/// Everything the director refuses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// The configuration is out of bounds.
    #[error("the catch-up configuration is invalid: {0}")]
    ConfigInvalid(&'static str),
    /// The gap summary or the planner settings are unusable (the planner's own code follows).
    #[error("the planner refused the gap: {0}")]
    Planner(&'static str),
    /// The progress store failed.
    #[error("the catch-up progress could not be stored")]
    Store,
    /// A chunk was reported out of order, or for a node with no bootstrap in progress.
    #[error("the chunk report does not match the bootstrap in progress")]
    ChunkOutOfOrder,
}

impl HostError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConfigInvalid(_) => "sync_catchup_host.config_invalid",
            Self::Planner(_) => "sync_catchup_host.planner_refused",
            Self::Store => "sync_catchup_host.store_failed",
            Self::ChunkOutOfOrder => "sync_catchup_host.chunk_out_of_order",
        }
    }
}
