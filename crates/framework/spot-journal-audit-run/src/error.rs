//! Typed errors with stable `journal_audit_run.*` codes.
//!
//! An error here is a refusal to run the audit at all: wrong wiring or unusable input. A rebuild
//! that fails (a gap, a missing upcaster, a broken chain) is not an error but an
//! [`AuditRun::Failed`](crate::AuditRun::Failed) result, because the schedule wants a result row
//! either way. No message carries record content, only entity names, ids and stable codes.

use spot_journal_audit::AuditError;

/// Why the audit could not be run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuditRunError {
    /// The options are unusable (empty source, empty tenant on a side).
    #[error("the audit options are not usable: {0}")]
    OptionsInvalid(&'static str),
    /// The live side or the scratch side belongs to another tenant than the run.
    #[error("the {side} side belongs to another tenant than this run")]
    TenantMismatch {
        /// Which side: `live` or `scratch`.
        side: &'static str,
    },
    /// The untrusted-snapshot path was asked for without a signer to mark the snapshot verified.
    #[error("the untrusted snapshot path needs a snapshot signer")]
    SignerMissing,
    /// A side's summaries or fingerprints were unusable (not sorted, or an invalid fingerprint).
    #[error("the comparison could not run: {0}")]
    CompareFailed(AuditError),
}

impl AuditRunError {
    /// The stable catalogue code of this refusal.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::OptionsInvalid(_) => "journal_audit_run.options_invalid",
            Self::TenantMismatch { .. } => "journal_audit_run.tenant_mismatch",
            Self::SignerMissing => "journal_audit_run.signer_missing",
            Self::CompareFailed(_) => "journal_audit_run.compare_failed",
        }
    }
}
