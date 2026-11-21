//! The `sync_bootstrap.*` error vocabulary (WP-SYN-07.T3).
//!
//! Every code here has one entry in `crates/framework/spot-errors/catalogue/sync_bootstrap.toml`.
//! Callers match on [`BootstrapError::code`], never on a message. Messages name entities, counts
//! and limits; they never carry a record image, a tenant id or a secret.

use thiserror::Error;

/// A limit or setting cannot work.
pub const INVALID_SETTINGS: &str = "sync_bootstrap.invalid_settings";
/// The snapshot is not marked trusted and the caller did not allow untrusted snapshots.
pub const UNTRUSTED_SNAPSHOT: &str = "sync_bootstrap.untrusted_snapshot";
/// The snapshot belongs to another tenant than the node.
pub const TENANT_MISMATCH: &str = "sync_bootstrap.tenant_mismatch";
/// The snapshot is not one the hub's current stream can hand over from.
pub const SNAPSHOT_MISMATCH: &str = "sync_bootstrap.snapshot_mismatch";
/// A link of a record could not be read by the catalog.
pub const REFERENCE_UNREADABLE: &str = "sync_bootstrap.reference_unreadable";
/// The snapshot source failed.
pub const SOURCE_FAILED: &str = "sync_bootstrap.source_failed";
/// The source returned a record the scope does not cover, or one that was not asked for.
pub const SCOPE_VIOLATION: &str = "sync_bootstrap.scope_violation";
/// A cycle of records cannot fit one bundle within the limits.
pub const COMPONENT_TOO_LARGE: &str = "sync_bootstrap.component_too_large";
/// The snapshot read now differs from the one the plan was made from.
pub const SNAPSHOT_CHANGED: &str = "sync_bootstrap.snapshot_changed";
/// A cursor does not belong to this session's snapshot, node and scope.
pub const CURSOR_MISMATCH: &str = "sync_bootstrap.cursor_mismatch";
/// A node asked for admission-controlled work it was not admitted to.
pub const NOT_ADMITTED: &str = "sync_bootstrap.not_admitted";
/// A position cannot advance without overflowing.
pub const POSITION_OVERFLOW: &str = "sync_bootstrap.position_overflow";

/// Why a bootstrap could not start, continue or be admitted.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BootstrapError {
    /// A tuning value cannot work.
    #[error("The bootstrap settings are not usable: {reason}.")]
    InvalidSettings {
        /// Which rule the settings break.
        reason: &'static str,
    },
    /// The snapshot is not trusted.
    #[error("The snapshot is not marked trusted, and untrusted snapshots were not allowed.")]
    UntrustedSnapshot,
    /// The snapshot is another tenant's.
    #[error("The snapshot belongs to a different tenant than the node.")]
    TenantMismatch,
    /// The snapshot does not fit the hub's stream.
    #[error("The snapshot cannot be used with the hub's stream: {reason}.")]
    SnapshotMismatch {
        /// Which rule the snapshot breaks.
        reason: &'static str,
    },
    /// The catalog could not read a record's links.
    #[error("The links of a {entity} record cannot be read from the catalog.")]
    ReferenceUnreadable {
        /// The record's entity.
        entity: String,
    },
    /// The snapshot source failed.
    #[error("The snapshot source failed: {reason}.")]
    SourceFailed {
        /// A short, secret-free description.
        reason: String,
    },
    /// The source returned something outside what was asked for or outside the scope.
    #[error("The snapshot source returned a {entity} record outside the node's scope or request.")]
    ScopeViolation {
        /// The record's entity.
        entity: String,
    },
    /// A cycle is larger than a bundle may be.
    #[error(
        "A group of {records} linked records ({bytes} bytes) cannot be split and does not fit one bundle."
    )]
    ComponentTooLarge {
        /// Records in the group.
        records: usize,
        /// Bytes in the group.
        bytes: usize,
    },
    /// The snapshot changed under the plan.
    #[error("The snapshot differs from the one the plan was made from: {reason}.")]
    SnapshotChanged {
        /// What differs.
        reason: &'static str,
    },
    /// The cursor is not this session's.
    #[error("The cursor does not belong to this snapshot, node and scope: {reason}.")]
    CursorMismatch {
        /// What differs.
        reason: &'static str,
    },
    /// The node was not admitted.
    #[error("The node is not admitted to bootstrap.")]
    NotAdmitted,
    /// A position cannot advance.
    #[error("A position cannot advance without overflowing.")]
    PositionOverflow,
}

impl BootstrapError {
    /// The stable dotted catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidSettings { .. } => INVALID_SETTINGS,
            Self::UntrustedSnapshot => UNTRUSTED_SNAPSHOT,
            Self::TenantMismatch => TENANT_MISMATCH,
            Self::SnapshotMismatch { .. } => SNAPSHOT_MISMATCH,
            Self::ReferenceUnreadable { .. } => REFERENCE_UNREADABLE,
            Self::SourceFailed { .. } => SOURCE_FAILED,
            Self::ScopeViolation { .. } => SCOPE_VIOLATION,
            Self::ComponentTooLarge { .. } => COMPONENT_TOO_LARGE,
            Self::SnapshotChanged { .. } => SNAPSHOT_CHANGED,
            Self::CursorMismatch { .. } => CURSOR_MISMATCH,
            Self::NotAdmitted => NOT_ADMITTED,
            Self::PositionOverflow => POSITION_OVERFLOW,
        }
    }
}
