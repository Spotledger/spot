//! Typed errors with stable `journal_privacy.*` codes. A message never carries a subject id, a
//! key or a personal value; it may name an entity and a field.

/// Every error of the crate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrivacyError {
    /// A personal field has no data-subject declaration: it could not be sealed, so it would
    /// reach the journal in clear. The metadata is refused.
    #[error("the personal field `{entity}.{path}` has no subject declaration")]
    SubjectMissing {
        /// The entity.
        entity: String,
        /// The field path.
        path: String,
    },
    /// A declaration cannot be built (a path that does not parse, an empty purpose).
    #[error("the declaration of `{entity}.{path}` is invalid: {reason}")]
    DeclarationInvalid {
        /// The entity.
        entity: String,
        /// The field path.
        path: String,
        /// Why, as a fixed sentence.
        reason: &'static str,
    },
    /// Personal fields exist but nothing can protect them: the journal must not ship them.
    #[error("personal fields are declared but protection is not available")]
    ProtectionMissing,
    /// The requester does not hold `journal.erase_subject`.
    #[error("the requester may not erase a data subject")]
    PermissionDenied,
    /// Every field of the subject is held under a retention that forbids erasing.
    #[error("`{entity}.{path}` is held under a retention class that forbids erasure")]
    RetentionForbids {
        /// The entity of the first retained field.
        entity: String,
        /// Its field path.
        path: String,
    },
    /// The subject id is empty.
    #[error("the data-subject id is empty")]
    SubjectInvalid,
    /// The key store (or the audit sink) failed: nothing is claimed as erased.
    #[error("the key store failed: {0}")]
    KeyStore(&'static str),
}

impl PrivacyError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::SubjectMissing { .. } => "journal_privacy.subject_missing",
            Self::DeclarationInvalid { .. } => "journal_privacy.declaration_invalid",
            Self::ProtectionMissing => "journal_privacy.protection_missing",
            Self::PermissionDenied => "journal_privacy.permission_denied",
            Self::RetentionForbids { .. } => "journal_privacy.retention_forbids",
            Self::SubjectInvalid => "journal_privacy.subject_invalid",
            Self::KeyStore(_) => "journal_privacy.key_store",
        }
    }
}
