//! What applying one received record did.

use crate::error::Reason;

/// The result of applying one received record (ADR-023, ADR-062 G4).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplyOutcome {
    /// The record was stored: its document, children and entries are durable.
    Stored,
    /// The same `(name, version)` is already stored; nothing changed (ADR-062 G4).
    Duplicate,
    /// A newer version is already stored; the older one is ignored (ADR-062 G4).
    Stale,
    /// The record was refused; the transaction rolled back and nothing was stored.
    Rejected(Reason),
}

impl ApplyOutcome {
    /// Whether the record was stored.
    #[must_use]
    pub fn is_stored(&self) -> bool {
        matches!(self, ApplyOutcome::Stored)
    }

    /// Whether nothing changed (a duplicate or a stale version).
    #[must_use]
    pub fn is_unchanged(&self) -> bool {
        matches!(self, ApplyOutcome::Duplicate | ApplyOutcome::Stale)
    }

    /// The rejection reason, when the record was refused.
    #[must_use]
    pub fn reason(&self) -> Option<&Reason> {
        match self {
            ApplyOutcome::Rejected(reason) => Some(reason),
            _ => None,
        }
    }

    /// The stable code of the rejection reason, when the record was refused.
    #[must_use]
    pub fn reason_code(&self) -> Option<&'static str> {
        self.reason().map(Reason::code)
    }
}
