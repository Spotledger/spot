//! What each `replica_apply.*` rejection code means to an operator.
//!
//! A quarantined item carries the apply path's own rejection code (WP-EDG-06.T1,
//! `replica_apply.toml`). This module **maps** that code to the queue's view of it: whether a
//! retry can help after the cause is fixed, how urgent it is, a one-sentence summary and the
//! suggested action. The words here are the platform's own; the catalogue holds the longer
//! developer-facing explanation.

use serde::{Deserialize, Serialize};
use spot_replica_apply::codes as apply;

use crate::retry::Severity;

/// The queue's view of one rejection code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonInfo {
    /// The `replica_apply.*` code this describes.
    pub code: String,
    /// Whether a retry can succeed once the cause is fixed (a metadata upgrade, storage health).
    pub retryable: bool,
    /// How urgent it is for a person.
    pub severity: Severity,
    /// One sentence in plain language.
    pub summary: String,
    /// The suggested action.
    pub remedy: String,
}

fn info(
    code: &str,
    retryable: bool,
    severity: Severity,
    summary: &str,
    remedy: &str,
) -> ReasonInfo {
    ReasonInfo {
        code: code.to_owned(),
        retryable,
        severity,
        summary: summary.to_owned(),
        remedy: remedy.to_owned(),
    }
}

/// The queue's view of a rejection code.
///
/// An unrecognised code is treated as not retryable and red, so a code this crate does not know
/// still reaches a person rather than being retried forever.
#[must_use]
pub fn describe(code: &str) -> ReasonInfo {
    match code {
        apply::NOT_OWNER => info(
            code,
            false,
            Severity::Red,
            "the sender does not own the record",
            "Check the ownership map and the sender's partition; an ownership violation is a bug or a misconfiguration.",
        ),
        apply::METADATA_SKEW => info(
            code,
            true,
            Severity::Amber,
            "the sender's metadata is newer than this node accepts",
            "Upgrade this node's metadata, then retry the item.",
        ),
        apply::UNKNOWN_ENTITY => info(
            code,
            true,
            Severity::Amber,
            "the record names an entity this node's catalog does not have",
            "Install the metadata that declares the entity, then retry.",
        ),
        apply::CHILD_ENTITY => info(
            code,
            false,
            Severity::Red,
            "the record names a child entity that travels inside its parent",
            "Fix the sender so child rows are sent inside their parent document.",
        ),
        apply::INVALID_ID => info(
            code,
            false,
            Severity::Red,
            "the record's id is not a UUIDv7",
            "Fix the sender's id generation; the record cannot be applied.",
        ),
        apply::MISSING_FIELD => info(
            code,
            true,
            Severity::Amber,
            "the record omits a declared field",
            "Send the full document, or install the metadata that declares the field.",
        ),
        apply::UNKNOWN_FIELD => info(
            code,
            true,
            Severity::Amber,
            "the record holds a field this node does not declare",
            "Align the sender's metadata with this node's, or install the metadata that declares the field, then retry.",
        ),
        apply::INVALID_VALUE => info(
            code,
            false,
            Severity::Red,
            "a field value does not fit its declared type",
            "Fix the value at the sender; shape is checked, business rules are not.",
        ),
        apply::UNKNOWN_CHILD_TABLE => info(
            code,
            true,
            Severity::Amber,
            "the record holds rows under an undeclared child table",
            "Align the sender's metadata, or install the metadata that declares the table, then retry.",
        ),
        apply::INVALID_PAYLOAD => info(
            code,
            false,
            Severity::Red,
            "the record payload could not be decoded",
            "Fix the sender's encoder; a payload the apply path cannot decode cannot be applied.",
        ),
        apply::INVALID_IDENTIFIER => info(
            code,
            false,
            Severity::Red,
            "the record's name, version or sequence is not in its documented form",
            "Fix the sender so every record has a non-empty name and a version and sequence of at least 1.",
        ),
        apply::INVALID_ORIGIN_FIELD => info(
            code,
            false,
            Severity::Red,
            "a configured origin field is not a storage name",
            "Configure origin field names that are valid storage names.",
        ),
        apply::DUPLICATE_ID => info(
            code,
            false,
            Severity::Red,
            "the bundle uses one record id for more than one record",
            "Fix the sender's bundle so every document and child row has its own id.",
        ),
        apply::WRITE_FAILED => info(
            code,
            true,
            Severity::Amber,
            "storing the record failed",
            "Check the storage backend's health; the transaction rolled back, so a retry is safe.",
        ),
        apply::AUDIT_FAILED => info(
            code,
            true,
            Severity::Amber,
            "recording the replicated audit marker failed",
            "Check the audit chain store's availability, then retry.",
        ),
        apply::NOTICE_FAILED => info(
            code,
            true,
            Severity::Amber,
            "appending the delivery notice failed",
            "Check the notice feed's availability, then retry.",
        ),
        crate::codes::PAYLOAD_TOO_LARGE => info(
            code,
            false,
            Severity::Red,
            "the record, its sender name or its reason code is longer than the queue stores, so only a marker was kept",
            "Ask the sender to split or shrink the record or fix the name; the marker can only be discarded.",
        ),
        crate::codes::DEPENDENT_HELD => info(
            code,
            true,
            Severity::Info,
            "a later version of a document waits behind a quarantined earlier version",
            "Resolve the earlier version; this item is released behind it.",
        ),
        _ => info(
            code,
            false,
            Severity::Red,
            "the record was refused with a reason this node does not recognise",
            "Inspect the item and the sender's software version.",
        ),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_mapping_classifies_known_and_unknown_codes() {
        let owner = describe(apply::NOT_OWNER);
        assert!(!owner.retryable, "an ownership violation is not a retry");
        assert_eq!(owner.severity, Severity::Red);

        let entity = describe(apply::UNKNOWN_ENTITY);
        assert!(
            entity.retryable,
            "installing the metadata makes a retry work"
        );
        assert_eq!(entity.severity, Severity::Amber);

        let write = describe(apply::WRITE_FAILED);
        assert!(write.retryable);
        assert_eq!(write.severity, Severity::Amber);

        let unknown = describe("replica_apply.something_new");
        assert!(!unknown.retryable);
        assert_eq!(unknown.severity, Severity::Red);
        assert_eq!(unknown.code, "replica_apply.something_new");
    }
}
