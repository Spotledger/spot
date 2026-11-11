//! The refusals of the adoption exchange.

use crate::Version;

/// Why an input was refused. A refusal never changes the machine's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AdoptionError {
    /// A local edit after the node stopped owning the record (the record is owned by the hub).
    #[error("the record is owned by Head Office and can no longer be edited here")]
    NotOwner,
    /// An acknowledgement whose final version is below a version the hub has already seen.
    #[error(
        "the acknowledgement's final version {final_version} is below the version {seen_version} already seen"
    )]
    StaleAck {
        /// The final version the acknowledgement carries.
        final_version: Version,
        /// The highest version the hub has seen from the node.
        seen_version: Version,
    },
    /// An acknowledgement that names versions the hub has not received: the stream order
    /// guarantee was broken, so the hub must not take ownership.
    #[error(
        "the acknowledgement's final version {final_version} is above the version {seen_version} the hub holds"
    )]
    MissingVersions {
        /// The final version the acknowledgement carries.
        final_version: Version,
        /// The highest version the hub has seen from the node.
        seen_version: Version,
    },
    /// An acknowledgement for a record no adoption request is pending on.
    #[error("no adoption request is pending for this record")]
    NoRequest,
    /// An acknowledgement that disagrees with the one that already completed the adoption.
    #[error("the record was already adopted with a different final version")]
    AckMismatch,
    /// An adoption trigger before the record has reached the hub: there is no version to name.
    #[error("the record has not reached the hub yet")]
    RecordNotArrived,
    /// A request that names a version the node never wrote.
    #[error("the adoption request names version {seen_version}, beyond the node's {node_version}")]
    RequestAhead {
        /// The version the request names.
        seen_version: Version,
        /// The node's own latest version.
        node_version: Version,
    },
    /// A version counter that cannot advance.
    #[error("the version counter is exhausted")]
    VersionExhausted,
}

impl AdoptionError {
    /// The stable machine-readable code (catalogue `sync_adoption.toml`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotOwner => "sync_adoption.not_owner",
            Self::StaleAck { .. } => "sync_adoption.stale_ack",
            Self::MissingVersions { .. } => "sync_adoption.missing_versions",
            Self::NoRequest => "sync_adoption.no_request",
            Self::AckMismatch => "sync_adoption.ack_mismatch",
            Self::RecordNotArrived => "sync_adoption.record_not_arrived",
            Self::RequestAhead { .. } => "sync_adoption.request_ahead",
            Self::VersionExhausted => "sync_adoption.version_exhausted",
        }
    }
}
