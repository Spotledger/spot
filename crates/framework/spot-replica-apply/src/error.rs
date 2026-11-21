//! Typed rejection reasons with stable [`codes`](crate::codes).

use crate::codes;

/// Why a received record was refused.
///
/// A `Rejected` outcome always carries one; the quarantine queue (WP-EDG-06.T2) stores the
/// [`code`](Reason::code) and shows its explanation. A reason never carries a secret: field
/// values are named by field, never echoed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Reason {
    /// The sender is not the record's owner (ADR-062 G1).
    #[error("the sender does not own the record; `{owner}` does")]
    NotOwner {
        /// The owner this node believes holds the record.
        owner: String,
    },
    /// A configured partition scope field holds no usable value.
    #[error(
        "the partition scope `{key_type}` cannot be read from field `{field}`: {why}; \
         ownership cannot be decided"
    )]
    InvalidScope {
        /// The scope key type the policy configures.
        key_type: String,
        /// The field that should supply its value.
        field: String,
        /// What is wrong (never the value itself).
        why: String,
    },
    /// The record's metadata version is outside the accepted skew (ADR-066).
    #[error(
        "the record's metadata version {record} is outside the accepted skew \
         (this node is at {local}, maximum skew {skew})"
    )]
    MetadataSkew {
        /// The sender's metadata version.
        record: u64,
        /// This node's catalog version.
        local: u64,
        /// The skew this node accepts.
        skew: u64,
    },
    /// The record names an entity the catalog does not have.
    #[error("`{entity}` is not an entity in this node's catalog")]
    UnknownEntity {
        /// The entity name.
        entity: String,
    },
    /// The record names a child entity.
    #[error("`{entity}` holds child rows; they travel inside their parent document")]
    ChildEntity {
        /// The child entity name.
        entity: String,
    },
    /// The record's id is not a valid UUIDv7 record id.
    #[error("`{id}` is not a valid record id (ADR-33: a UUIDv7)")]
    InvalidId {
        /// The id as it arrived.
        id: String,
    },
    /// The record omits a declared field.
    #[error("`{entity}` needs field `{field}`; a replicated record stores every declared field")]
    MissingField {
        /// The entity.
        entity: String,
        /// The missing field.
        field: String,
    },
    /// The record holds an undeclared field.
    #[error("`{entity}` has no field `{field}`")]
    UnknownField {
        /// The entity.
        entity: String,
        /// The undeclared field.
        field: String,
    },
    /// A field value does not fit its declared type.
    #[error("`{entity}.{field}`: {why}")]
    InvalidValue {
        /// The entity.
        entity: String,
        /// The field.
        field: String,
        /// What is wrong with the value (never the value itself).
        why: String,
    },
    /// The record holds rows under an undeclared child-table field.
    #[error("`{entity}` has no child table `{table}`")]
    UnknownChildTable {
        /// The entity.
        entity: String,
        /// The undeclared child-table field.
        table: String,
    },
    /// The record image could not be decoded from its payload.
    #[error("the record payload is not valid: {why}")]
    InvalidPayload {
        /// What is wrong with the payload.
        why: String,
    },
    /// The record's name, version or sequence is not in its documented form.
    #[error("the record has an invalid {what}")]
    InvalidIdentifier {
        /// Which identifier (name, version, sequence).
        what: String,
    },
    /// A configured origin field name is not a storage name.
    #[error("the configured origin field `{field}` is not a storage name")]
    InvalidOriginField {
        /// The configured field name.
        field: String,
    },
    /// The bundle uses one record id for two records.
    #[error("the bundle uses record id {id} for more than one record")]
    DuplicateId {
        /// The reused id.
        id: String,
    },
    /// Writing the bundle failed.
    #[error("storing the record failed: {why}")]
    WriteFailed {
        /// What storage reported.
        why: String,
    },
    /// Recording the `replicated` audit marker failed.
    #[error("recording the replicated audit marker failed: {why}")]
    AuditFailed {
        /// What the audit sink reported.
        why: String,
    },
    /// Appending the delivery notice failed.
    #[error("appending the delivery notice failed: {why}")]
    NoticeFailed {
        /// What the notice sink reported.
        why: String,
    },
    /// Handing a refused record to the quarantine port failed.
    #[error("handing the refused record to the quarantine queue failed: {why}")]
    QuarantineFailed {
        /// What the port reported.
        why: String,
    },
}

impl Reason {
    /// Whether this reason is an **ownership** refusal, which the quarantine port holds.
    ///
    /// A structural or storage problem is not: the queue's intake adapter quarantines those on
    /// its own, and the apply path's ownership handoff is only for ADR-062 G1's decision.
    #[must_use]
    pub fn is_ownership_refusal(&self) -> bool {
        matches!(self, Reason::NotOwner { .. } | Reason::InvalidScope { .. })
    }

    /// The stable code from [`codes`](crate::codes) that names this reason.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Reason::NotOwner { .. } => codes::NOT_OWNER,
            Reason::InvalidScope { .. } => codes::INVALID_SCOPE,
            Reason::MetadataSkew { .. } => codes::METADATA_SKEW,
            Reason::UnknownEntity { .. } => codes::UNKNOWN_ENTITY,
            Reason::ChildEntity { .. } => codes::CHILD_ENTITY,
            Reason::InvalidId { .. } => codes::INVALID_ID,
            Reason::MissingField { .. } => codes::MISSING_FIELD,
            Reason::UnknownField { .. } => codes::UNKNOWN_FIELD,
            Reason::InvalidValue { .. } => codes::INVALID_VALUE,
            Reason::UnknownChildTable { .. } => codes::UNKNOWN_CHILD_TABLE,
            Reason::InvalidPayload { .. } => codes::INVALID_PAYLOAD,
            Reason::InvalidIdentifier { .. } => codes::INVALID_IDENTIFIER,
            Reason::InvalidOriginField { .. } => codes::INVALID_ORIGIN_FIELD,
            Reason::DuplicateId { .. } => codes::DUPLICATE_ID,
            Reason::WriteFailed { .. } => codes::WRITE_FAILED,
            Reason::AuditFailed { .. } => codes::AUDIT_FAILED,
            Reason::NoticeFailed { .. } => codes::NOTICE_FAILED,
            Reason::QuarantineFailed { .. } => codes::QUARANTINE_FAILED,
        }
    }
}
