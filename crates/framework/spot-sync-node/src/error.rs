//! The node error vocabulary and its stable catalogue codes (`catalogue/sync_node.toml`).

use thiserror::Error;

/// A delivery came from a source other than the configured hub.
pub const FOREIGN_SOURCE: &str = "sync_node.foreign_source";
/// A record image could not be read for its references.
pub const REFERENCE_UNREADABLE: &str = "sync_node.reference_unreadable";
/// A delivery names a new tenant beyond the node's tenant cap.
pub const TOO_MANY_TENANTS: &str = "sync_node.too_many_tenants";
/// A parking limit is zero.
pub const INVALID_LIMITS: &str = "sync_node.invalid_limits";

/// Why a delivery could not be taken in at all (as opposed to a [`crate::Receipt`] outcome).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NodeError {
    /// The delivery's source is not the hub this node is configured for. Nothing was parked.
    #[error("A delivery came from {source_id}, which is not the hub this node is configured for.")]
    ForeignSource {
        /// The source the delivery claimed.
        source_id: String,
    },
    /// A record image could not be read for its references.
    #[error("A {entity} record has a link in {field} whose target cannot be determined.")]
    ReferenceUnreadable {
        /// The entity whose image was read.
        entity: String,
        /// The link field, or `unknown` when the catalog did not say.
        field: String,
    },
    /// A delivery names a new tenant beyond the cap. Nothing was stored.
    #[error(
        "The node already serves its maximum of {limit} tenants; a delivery for a new tenant was refused."
    )]
    TooManyTenants {
        /// The configured cap.
        limit: usize,
    },
    /// A parking limit is zero.
    #[error("The parking limits are not usable: {reason}.")]
    InvalidLimits {
        /// Which rule the limits break.
        reason: &'static str,
    },
}

impl NodeError {
    /// The stable dotted catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ForeignSource { .. } => FOREIGN_SOURCE,
            Self::ReferenceUnreadable { .. } => REFERENCE_UNREADABLE,
            Self::TooManyTenants { .. } => TOO_MANY_TENANTS,
            Self::InvalidLimits { .. } => INVALID_LIMITS,
        }
    }
}
