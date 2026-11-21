//! Typed errors for `spot-sync-reconcile`: stable `sync_reconcile.*` codes.
//!
//! Callers match on [`ReconcileError::code`], never on the message: the codes are this crate's
//! API contract and every one is catalogued in
//! `crates/framework/spot-errors/catalogue/sync_reconcile.toml`.

use spot_app_api::error::{AppError, ErrorClass};

/// What can go wrong in a reconciliation cycle.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReconcileError {
    /// A repair was planned with a mirror as the source of truth. Only an owner may be repaired
    /// from; a mirror is never repaired from a mirror (ADR-062 G1, ADR-067 rule 6).
    #[error("cannot plan repairs with a mirror as the source of truth: {entity}/{name}")]
    MirrorAsSource {
        /// The entity of the record whose repair was requested.
        entity: String,
        /// The name of the record whose repair was requested.
        name: String,
    },

    /// The cycle asked a source for a scope it does not hold.
    #[error("invalid scope: {reason}")]
    InvalidScope {
        /// What was wrong, without echoing a secret.
        reason: String,
    },

    /// The cycle's schedule settings do not parse.
    #[error("invalid reconciliation schedule: {reason}")]
    InvalidSchedule {
        /// What was wrong, without echoing a secret.
        reason: String,
    },

    /// The digest plan names an entity the node's catalog does not hold.
    #[error("the digest plan names an unknown entity: {entity}")]
    UnknownEntity {
        /// The entity the plan named.
        entity: String,
    },

    /// Reading a node's storage failed.
    #[error("reading the node's storage failed: {message}")]
    Source {
        /// What the backend reported, without a raw SQL string or a secret.
        message: String,
    },
}

impl ReconcileError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::MirrorAsSource { .. } => "sync_reconcile.mirror_as_source",
            Self::InvalidScope { .. } => "sync_reconcile.invalid_scope",
            Self::InvalidSchedule { .. } => "sync_reconcile.invalid_schedule",
            Self::UnknownEntity { .. } => "sync_reconcile.unknown_entity",
            Self::Source { .. } => "sync_reconcile.source_failed",
        }
    }

    /// The error as an [`AppError`], so a cycle reported through the platform carries a code
    /// the engine and the catalogue know.
    #[must_use]
    pub fn into_app_error(self) -> AppError {
        let class = match self {
            Self::InvalidScope { .. }
            | Self::InvalidSchedule { .. }
            | Self::UnknownEntity { .. } => ErrorClass::Validation,
            Self::MirrorAsSource { .. } | Self::Source { .. } => ErrorClass::Internal,
        };
        AppError::new(class, self.code(), self.to_string())
    }
}

impl From<ReconcileError> for AppError {
    fn from(error: ReconcileError) -> Self {
        error.into_app_error()
    }
}

/// The result of a fallible reconciliation step.
pub type ReconcileResult<T> = Result<T, ReconcileError>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn codes_are_stable_and_unique() {
        let errors = [
            ReconcileError::MirrorAsSource {
                entity: "item".into(),
                name: "A".into(),
            },
            ReconcileError::InvalidScope { reason: "x".into() },
            ReconcileError::InvalidSchedule { reason: "x".into() },
            ReconcileError::UnknownEntity {
                entity: "item".into(),
            },
            ReconcileError::Source {
                message: "x".into(),
            },
        ];
        let mut seen = std::collections::BTreeSet::new();
        for error in &errors {
            let code = error.code();
            assert!(code.starts_with("sync_reconcile."), "{code}");
            assert!(seen.insert(code), "duplicate code {code}");
        }
        assert_eq!(seen.len(), 5);
    }
}
