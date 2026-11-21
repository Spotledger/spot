//! Typed, explained storage errors (ADR-26).

use std::fmt;

use crate::id::Name;

/// Result of every storage operation.
pub type StorageResult<T> = Result<T, StorageError>;

/// What went wrong, as a stable, matchable kind.
///
/// Every backend maps its native failures onto these kinds; callers never parse messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The record or document does not exist.
    NotFound,
    /// A record with this id already exists (insert of an existing id, or a child row id used
    /// by another document).
    AlreadyExists,
    /// The document changed since it was read: the expected version is not the stored one.
    StaleVersion,
    /// A unique index would hold two equal keys.
    UniqueViolation,
    /// A range-exclusion constraint ([`ExclusionDef`](crate::ExclusionDef)) would hold two
    /// overlapping ranges in the same scope (ADR-087 section 5.6). `entity`, `constraint` and
    /// `field` name what conflicted. The conflicting record's id is carried only when the caller
    /// may read that record; storage cannot know permission, so a storage backend never sets
    /// one, and the layer above adds it after its own read check. Otherwise the explanation says
    /// only that the dates conflict with an existing record, with no id and no field values.
    ExclusionViolation,
    /// A database-checked link ([`LinkCheck::Database`](crate::LinkCheck)) points at a record
    /// that does not exist, or a record is deleted while such a link still points at it. May be
    /// reported by the write or, at the latest, by commit; `entity` and `field` name the link.
    ReferenceViolation,
    /// A value does not fit its field: wrong type, null in a required field, a decimal with more
    /// fractional digits than the field's scale, text longer than its limit, invalid JSON, a
    /// missing or unknown field in a write, a coordinate out of range, an empty, unbounded or
    /// not half-open range.
    InvalidValue,
    /// A name is not a valid storage identifier.
    InvalidName,
    /// The catalog is inconsistent (unknown child entity, link to a missing entity, reserved or
    /// duplicate field name).
    InvalidSchema,
    /// The entity is not in the transaction's catalog.
    UnknownEntity,
    /// The field is not defined on the entity.
    UnknownField,
    /// The query is not valid against the catalog (bad alias, comparison with null, aggregate
    /// misuse, lock on an aggregate query).
    InvalidQuery,
    /// The transaction could not be serialized with concurrent ones. Retry the whole transaction.
    SerializationFailure,
    /// The transaction was chosen as a deadlock victim. Retry the whole transaction.
    Deadlock,
    /// A lock was not granted within the transaction's lock timeout.
    LockTimeout,
    /// A lock requested with [`Wait::NoWait`](crate::Wait::NoWait) is held by another transaction.
    LockNotAvailable,
    /// A statement exceeded the transaction's statement timeout.
    StatementTimeout,
    /// The transaction exceeded its declared write limit (ADR-45); chunk the work.
    TransactionTooLarge,
    /// A write was attempted in a read-only transaction.
    ReadOnlyTransaction,
    /// A traversal reached more nodes than its limit allows.
    TraversalTooLarge,
    /// A feed position can no longer be continued: it is older than the retention, from another
    /// stream generation (for example after a restore), or beyond the end of the feed. The
    /// consumer must resynchronise (rebuild its state from a snapshot and reconcile effects).
    ResyncRequired,
    /// The backend does not offer this capability (for example no read replica).
    Unsupported,
    /// The backend cannot be reached. Safe to retry: nothing was committed.
    Unavailable,
    /// The connection failed during commit: the transaction may or may not have committed.
    /// Never retry blindly; check the outcome first (for example by reloading the document).
    CommitOutcomeUnknown,
    /// A bug or an unexpected backend state.
    Internal,
}

impl ErrorKind {
    /// A stable code for logs, APIs and support tooling.
    pub fn code(self) -> &'static str {
        match self {
            ErrorKind::NotFound => "storage.not_found",
            ErrorKind::AlreadyExists => "storage.already_exists",
            ErrorKind::StaleVersion => "storage.stale_version",
            ErrorKind::UniqueViolation => "storage.unique_violation",
            ErrorKind::ExclusionViolation => "storage.exclusion_violation",
            ErrorKind::ReferenceViolation => "storage.reference_violation",
            ErrorKind::InvalidValue => "storage.invalid_value",
            ErrorKind::InvalidName => "storage.invalid_name",
            ErrorKind::InvalidSchema => "storage.invalid_schema",
            ErrorKind::UnknownEntity => "storage.unknown_entity",
            ErrorKind::UnknownField => "storage.unknown_field",
            ErrorKind::InvalidQuery => "storage.invalid_query",
            ErrorKind::SerializationFailure => "storage.serialization_failure",
            ErrorKind::Deadlock => "storage.deadlock",
            ErrorKind::LockTimeout => "storage.lock_timeout",
            ErrorKind::LockNotAvailable => "storage.lock_not_available",
            ErrorKind::StatementTimeout => "storage.statement_timeout",
            ErrorKind::TransactionTooLarge => "storage.transaction_too_large",
            ErrorKind::ReadOnlyTransaction => "storage.read_only_transaction",
            ErrorKind::TraversalTooLarge => "storage.traversal_too_large",
            ErrorKind::ResyncRequired => "storage.resync_required",
            ErrorKind::Unsupported => "storage.unsupported",
            ErrorKind::Unavailable => "storage.unavailable",
            ErrorKind::CommitOutcomeUnknown => "storage.commit_outcome_unknown",
            ErrorKind::Internal => "storage.internal",
        }
    }

    /// Whether rerunning the whole transaction from the start may succeed (ADR-46: serialization
    /// failures are retried with backoff). Retrying is only safe when the transaction has no
    /// effects outside the database, which ADR-51 guarantees for in-transaction hooks.
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            ErrorKind::SerializationFailure | ErrorKind::Deadlock | ErrorKind::Unavailable
        )
    }
}

/// A storage failure with its kind, a plain-language explanation and the names involved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageError {
    /// The matchable kind.
    pub kind: ErrorKind,
    /// What happened and what to do about it, in plain language.
    pub explanation: String,
    /// The entity involved, if any.
    pub entity: Option<Name>,
    /// The field involved, if any.
    pub field: Option<Name>,
    /// The index or constraint involved, if any.
    pub constraint: Option<Name>,
}

impl StorageError {
    /// A new error of `kind`.
    pub fn new(kind: ErrorKind, explanation: impl Into<String>) -> Self {
        Self {
            kind,
            explanation: explanation.into(),
            entity: None,
            field: None,
            constraint: None,
        }
    }

    /// Names the entity involved.
    pub fn with_entity(mut self, entity: &Name) -> Self {
        self.entity = Some(entity.clone());
        self
    }

    /// Names the field involved.
    pub fn with_field(mut self, field: &Name) -> Self {
        self.field = Some(field.clone());
        self
    }

    /// Names the index or constraint involved.
    pub fn with_constraint(mut self, constraint: &Name) -> Self {
        self.constraint = Some(constraint.clone());
        self
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.code(), self.explanation)?;
        if let Some(e) = &self.entity {
            write!(f, " (entity {e}")?;
            if let Some(field) = &self.field {
                write!(f, ", field {field}")?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}

impl std::error::Error for StorageError {}
