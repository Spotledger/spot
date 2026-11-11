//! The stable `replica_apply.*` rejection codes of the replication apply path.
//!
//! Every code here has one entry in `spot-errors`' `catalogue/replica_apply.toml` with its
//! class, a one-sentence summary and a remedy. A code is a **rejection reason**: it says why a
//! received record was not stored, and the quarantine queue (WP-EDG-06.T2) maps it to the
//! reason it shows an operator.

/// The sender is not the record's owner according to this node's view (ADR-062 G1, ADR-063).
pub const NOT_OWNER: &str = "replica_apply.not_owner";
/// A configured partition scope field holds no usable value, so ownership cannot be decided.
pub const INVALID_SCOPE: &str = "replica_apply.invalid_scope";
/// The record's metadata version is outside the skew this node accepts (ADR-066).
pub const METADATA_SKEW: &str = "replica_apply.metadata_skew";
/// The record names an entity this node's catalog does not have.
pub const UNKNOWN_ENTITY: &str = "replica_apply.unknown_entity";
/// The record names a child entity; child rows travel inside their parent (ADR-063 L5).
pub const CHILD_ENTITY: &str = "replica_apply.child_entity";
/// The record's id is not a valid UUIDv7 record id (ADR-33).
pub const INVALID_ID: &str = "replica_apply.invalid_id";
/// The record omits a declared field of its entity.
pub const MISSING_FIELD: &str = "replica_apply.missing_field";
/// The record holds a field its entity does not declare.
pub const UNKNOWN_FIELD: &str = "replica_apply.unknown_field";
/// A field value does not fit its declared type (shape, not a business rule).
pub const INVALID_VALUE: &str = "replica_apply.invalid_value";
/// The record holds rows under a child-table field its entity does not declare.
pub const UNKNOWN_CHILD_TABLE: &str = "replica_apply.unknown_child_table";
/// The record image could not be decoded from its payload.
pub const INVALID_PAYLOAD: &str = "replica_apply.invalid_payload";
/// The record's name, version or sequence is not in its documented form.
pub const INVALID_IDENTIFIER: &str = "replica_apply.invalid_identifier";
/// A configured origin field name is not a storage name.
pub const INVALID_ORIGIN_FIELD: &str = "replica_apply.invalid_origin_field";
/// The bundle uses one record id for two records.
pub const DUPLICATE_ID: &str = "replica_apply.duplicate_id";
/// Writing the bundle through the storage contract failed.
pub const WRITE_FAILED: &str = "replica_apply.write_failed";
/// Recording the `replicated` audit marker failed.
pub const AUDIT_FAILED: &str = "replica_apply.audit_failed";
/// Appending the delivery notice failed.
pub const NOTICE_FAILED: &str = "replica_apply.notice_failed";
/// Handing a refused record to the quarantine port failed.
pub const QUARANTINE_FAILED: &str = "replica_apply.quarantine_failed";
