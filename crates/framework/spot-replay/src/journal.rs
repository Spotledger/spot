//! The journal: the committed operations a replay samples from, and the seam that reads them.
//!
//! A [`ReplaySource`] is the read-only half of a replay. It yields [`JournalEntry`] items — one
//! committed operation each, with what the **old** release recorded as its outcome — in a
//! deterministic order (causal order within a source, sources in a fixed order). A replay never
//! writes to the source and never reads the live database directly: the source is a reader over
//! the site's committed event journal / outbox (ADR-061), the same feed the job intake reads.
//!
//! The engine owns the *shape*, not the contents: Foundation and ERP supply the reader (the real
//! one is a page over the outbox feed); the tests supply a fake. Time is never read from a clock:
//! each entry carries the instant the original operation happened at, which a replay fixes its
//! clock to.

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::{OffsetDateTime, RecordId, Version};
use spot_app_api::{Actor, Document, DocumentPatch};

use crate::BoxFuture;
use crate::error::Result;

/// How sensitive an operation's DocType is. The sample is stratified by it, so a replay always
/// covers the tax, valuation and GL-posting operations (blueprint WS12: `auto` replay).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    /// An ordinary document.
    Normal,
    /// A high-sensitivity document: tax, valuation or a GL posting.
    High,
}

/// The operation a journal entry recorded.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplayOperation {
    /// Create a document.
    Create {
        /// The document as it was created.
        document: Document,
    },
    /// Update a draft.
    Update {
        /// The fields the update set.
        patch: DocumentPatch,
        /// The version the update expected.
        expected: Version,
    },
    /// Post a draft.
    Post {
        /// The version the post expected.
        expected: Version,
    },
    /// Cancel a posted document.
    Cancel {
        /// The version the cancel expected.
        expected: Version,
    },
    /// Delete a draft.
    Delete {
        /// The version the delete expected.
        expected: Version,
    },
}

/// The operation kind, without its payload. Used in reports and for the causal order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// A create.
    Create,
    /// An update.
    Update,
    /// A post.
    Post,
    /// A cancel.
    Cancel,
    /// A delete.
    Delete,
}

impl ReplayOperation {
    /// The operation's kind.
    #[must_use]
    pub fn kind(&self) -> OperationKind {
        match self {
            ReplayOperation::Create { .. } => OperationKind::Create,
            ReplayOperation::Update { .. } => OperationKind::Update,
            ReplayOperation::Post { .. } => OperationKind::Post,
            ReplayOperation::Cancel { .. } => OperationKind::Cancel,
            ReplayOperation::Delete { .. } => OperationKind::Delete,
        }
    }
}

/// The principal an operation was performed by: the identity whose rights applied, and the roles
/// it held. A replay re-runs the operation **as this principal**, so a permission change is as
/// visible as a rounding change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// The actor: the user (or the platform component).
    pub actor: Actor,
    /// The user's record id, when the actor is a user.
    pub user: Option<RecordId>,
    /// The roles the principal held at the time.
    pub roles: Vec<String>,
}

impl Principal {
    /// A principal for `actor` with `roles`.
    #[must_use]
    pub fn new(actor: Actor, user: Option<RecordId>, roles: Vec<String>) -> Self {
        Principal { actor, user, roles }
    }

    /// The platform acting for itself, with no roles.
    #[must_use]
    pub fn system(component: spot_app_api::prelude::Name) -> Self {
        Principal {
            actor: Actor::System { component },
            user: None,
            roles: Vec::new(),
        }
    }
}

/// One committed operation, with what the old release recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct JournalEntry {
    /// The ordering group: operations of one source keep their relative order (causal order per
    /// source). Effects of one document share a source.
    pub source: String,
    /// The position within the source, increasing.
    pub sequence: u64,
    /// The DocType the operation was about.
    pub doctype: String,
    /// The document's record id.
    pub record: RecordId,
    /// The operation to re-execute.
    pub operation: ReplayOperation,
    /// The instant the original operation happened at; a replay fixes its clock to it.
    pub occurred_at: OffsetDateTime,
    /// The principal the operation ran as.
    pub principal: Principal,
    /// The DocType's sensitivity, for stratification.
    pub sensitivity: Sensitivity,
    /// What the old release recorded as the outcome.
    pub recorded: crate::Outcome,
}

impl JournalEntry {
    /// The `(source, sequence)` key a causal order sorts by.
    #[must_use]
    pub fn order_key(&self) -> (&str, u64) {
        (self.source.as_str(), self.sequence)
    }

    /// The record id as the canonical text form a report and an outcome carry.
    #[must_use]
    pub fn record_text(&self) -> String {
        self.record.to_string()
    }
}

/// A resumable position in the journal: the last `(source, sequence)` a replay completed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Cursor {
    /// The source.
    pub source: String,
    /// The sequence within the source.
    pub sequence: u64,
}

impl Cursor {
    /// A cursor at `(source, sequence)`.
    #[must_use]
    pub fn new(source: impl Into<String>, sequence: u64) -> Self {
        Cursor {
            source: source.into(),
            sequence,
        }
    }

    /// The cursor of an entry.
    #[must_use]
    pub fn of(entry: &JournalEntry) -> Self {
        Cursor {
            source: entry.source.clone(),
            sequence: entry.sequence,
        }
    }
}

/// One page of the journal.
#[derive(Clone, Debug, PartialEq)]
pub struct JournalPage {
    /// The entries, in causal order, strictly after the cursor the page was read from.
    pub entries: Vec<JournalEntry>,
    /// The cursor to resume from: the last entry of the page, when the page is not empty.
    pub next: Option<Cursor>,
    /// Whether the source has no more entries after this page.
    pub exhausted: bool,
}

/// The read-only journal reader a replay samples from.
///
/// A source returns entries in a deterministic order — causal order within a source, sources in a
/// fixed order — and honours a cursor, so a replay stopped by its budget resumes exactly where it
/// stopped. Reading is the only thing a source does.
pub trait ReplaySource: Send + Sync {
    /// The entries after `cursor` (or from the start when `None`), at most `limit` of them.
    fn read<'a>(
        &'a self,
        cursor: Option<&'a Cursor>,
        limit: usize,
    ) -> BoxFuture<'a, Result<JournalPage>>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn cursor_orders_lexicographically_by_source_then_sequence() {
        let mut cursors = vec![
            Cursor::new("b", 1),
            Cursor::new("a", 2),
            Cursor::new("a", 1),
        ];
        cursors.sort();
        assert_eq!(
            cursors,
            vec![
                Cursor::new("a", 1),
                Cursor::new("a", 2),
                Cursor::new("b", 1)
            ]
        );
    }

    #[test]
    fn operation_kind_matches_the_payload() {
        let doc = Document::new(
            spot_app_api::prelude::Name::new("invoice").expect("name"),
            RecordId::new_v7(),
        );
        let op = ReplayOperation::Create { document: doc };
        assert_eq!(op.kind(), OperationKind::Create);
        assert_eq!(
            ReplayOperation::Delete {
                expected: Version::FIRST
            }
            .kind(),
            OperationKind::Delete
        );
    }

    #[test]
    fn a_system_principal_has_no_roles() {
        let principal =
            Principal::system(spot_app_api::prelude::Name::new("migration").expect("name"));
        assert!(principal.roles.is_empty());
        assert!(principal.user.is_none());
    }
}
