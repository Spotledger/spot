//! The one error type of this crate, with a stable `sync_health.*` code.
//!
//! Messages carry the node id only, never a record or a payload.

/// The catalogue code of [`HealthError::NegativeContactTime`].
pub const NEGATIVE_CONTACT_TIME: &str = "sync_health.negative_contact_time";

/// A refusal to turn a ledger row into node facts.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HealthError {
    /// The row's last contact time is before the Unix epoch, so it is not hub time.
    #[error("node {node} has a negative last contact time")]
    NegativeContactTime {
        /// The node id as given.
        node: String,
    },
}

impl HealthError {
    /// The stable machine-readable code (catalogued in `catalogue/sync_health.toml`).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NegativeContactTime { .. } => NEGATIVE_CONTACT_TIME,
        }
    }
}
