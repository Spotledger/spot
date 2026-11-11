//! The platform's lifecycle topics (ADR-51, phase 4 "notify").
//!
//! The document engine enqueues one of these events in the same transaction as every
//! successful lifecycle operation (ADR-11). After commit the outbox delivers it to the
//! operation's `*_committed` handlers, which is why those handlers never roll a document back
//! and must be idempotent. `save_committed` handlers subscribe to both `doc_created` and
//! `doc_updated`.
//!
//! | Operation | Topic | `*_committed` hook |
//! | --- | --- | --- |
//! | create | `doc_created` | `create_committed` |
//! | update | `doc_updated` | `update_committed` |
//! | post | `doc_posted` | `post_committed` |
//! | cancel | `doc_cancelled` | `cancel_committed` |
//! | delete | `doc_deleted` | `delete_committed` |
//! | rename | `doc_renamed` | `rename_committed` |
//! | update_posted | `doc_updated_posted` | `update_posted_committed` |
//!
//! The subject is the document; `subject_version` is the version the operation produced (for
//! a delete, the last version before deletion). The same names are the realtime event kinds of
//! the native API (ADR-055), which are published from these events.

use std::collections::{BTreeMap, BTreeSet};

use crate::names::Topic;
use crate::schema::{EventSchema, FieldSchema, PayloadType};

/// A lifecycle operation (ADR-51).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    /// Create.
    Create,
    /// Update of a draft.
    Update,
    /// Post (ADR-42).
    Post,
    /// Cancel a posted document.
    Cancel,
    /// Delete.
    Delete,
    /// Rename (the human-readable name; the id never changes).
    Rename,
    /// Update of fields allowed to change after posting.
    UpdatePosted,
}

impl Operation {
    /// Every operation.
    pub const ALL: [Operation; 7] = [
        Operation::Create,
        Operation::Update,
        Operation::Post,
        Operation::Cancel,
        Operation::Delete,
        Operation::Rename,
        Operation::UpdatePosted,
    ];

    /// The topic name of this operation's event.
    pub fn topic_name(self) -> &'static str {
        match self {
            Operation::Create => "doc_created",
            Operation::Update => "doc_updated",
            Operation::Post => "doc_posted",
            Operation::Cancel => "doc_cancelled",
            Operation::Delete => "doc_deleted",
            Operation::Rename => "doc_renamed",
            Operation::UpdatePosted => "doc_updated_posted",
        }
    }

    /// The topic of this operation's event.
    pub fn topic(self) -> Topic {
        // The names above are fixed, valid storage names; the test suite checks each one.
        match Topic::any(self.topic_name()) {
            Ok(t) => t,
            Err(e) => unreachable!("lifecycle topic {} is invalid: {e}", self.topic_name()),
        }
    }

    /// The v1 payload schema of this operation's event.
    ///
    /// - `name`: the document's human-readable name or number at the time, when it has one.
    /// - `changed_fields`: for updates, the top-level fields (and child tables) that changed.
    /// - `old_name` / `new_name`: for renames, both required.
    pub fn schema(self) -> EventSchema {
        let text = || PayloadType::Text;
        let mut fields = BTreeMap::new();
        match self {
            Operation::Rename => {
                fields.insert("old_name".to_owned(), FieldSchema::required(text()));
                fields.insert("new_name".to_owned(), FieldSchema::required(text()));
            }
            Operation::Update | Operation::UpdatePosted => {
                fields.insert("name".to_owned(), FieldSchema::optional(text()));
                fields.insert(
                    "changed_fields".to_owned(),
                    FieldSchema::required(PayloadType::List(Box::new(text()))),
                );
            }
            _ => {
                fields.insert("name".to_owned(), FieldSchema::optional(text()));
            }
        }
        EventSchema {
            topic: self.topic(),
            major: 1,
            revision: 0,
            fields,
            retired: BTreeSet::new(),
        }
    }
}

/// The v1 schemas of every lifecycle topic.
pub fn schemas() -> Vec<EventSchema> {
    Operation::ALL.iter().map(|op| op.schema()).collect()
}
