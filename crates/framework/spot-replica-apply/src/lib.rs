//! The replication apply path (WP-EDG-06.T1, ADR-062, ADR-058).
//!
//! Sync, rebuild from the journal and tenant links share **one** apply path: it stores a received
//! document version, with its child tables and the ledger and stock entries it produced, **without
//! re-running business logic or side effects and without emitting a new event** (ADR-062 section
//! 2, ADR-023).
//!
//! # What it does
//!
//! [`apply`] takes the caller's transaction, a [`ReceivedRecord`] and an [`ApplyCtx`], and:
//!
//! - validates the record's **shape** against the storage catalog (entity kind, exact fields,
//!   value types) — never a business rule;
//! - checks the metadata skew and, before any write, that the sender owns the record's partition
//!   at the item's instant ([`PartitionOwners`], ADR-062 G1); an ownership refusal is handed to
//!   the quarantine port ([`QuarantinePort`]) and never stored;
//! - is **idempotent by name and version** (ADR-062 G4): a duplicate is
//!   [`ApplyOutcome::Duplicate`] and an older version is [`ApplyOutcome::Stale`], both without
//!   writing;
//! - writes the bundle through the storage contract, marking a mirror with its origin
//!   (ADR-058 rule 5) so it is read-only locally;
//! - appends a delivery notice for a `notify` topic (ADR-058 rule 9) and writes a `replicated`
//!   audit marker through the audit chain (ADR-061);
//! - commits, or rolls back: the bundle commits or nothing does (ADR-062 G3).
//!
//! # Ownership is checked, not trusted
//!
//! [`PartitionOwners`] presents the decoded record to `spot-partition` as a
//! [`ReplicaRecord`](spot_partition::ReplicaRecord) ([`ReceivedRecordView`]) and asks its
//! [`OwnerCheck::allow_replica`](spot_partition::OwnerCheck::allow_replica) over the ownership map
//! at [`ApplyCtx::at`]. The scope keys a document carries come from a [`ScopeSource`]
//! ([`ScopeFields`] by default empty): ADR-063's `owner_key` is still proposed, so which field
//! supplies which key type is configuration, never a rule in the apply path.
//!
//! # What it never does
//!
//! It never calls a controller, a lifecycle hook, a server script, a naming series, a
//! business-rule validation or the outbox emitter, and it never enqueues an outbox event: a
//! replicated record is not a new change at this node. A [`SideEffects`] sentinel passed in the
//! context is never touched; the test suite passes a panicking one to prove it.
//!
//! # Policy is elsewhere
//!
//! Ownership, whether a topic notifies, how much metadata skew is accepted and where the origin
//! lives are all behind traits or plain values in [`ApplyCtx`], so the later sync ADRs (063 to
//! 067) change policy without touching this path.
//!
//! # Time and storage are arguments
//!
//! No function reads the wall clock or an ambient random source; the instant is
//! [`ApplyCtx::at`]. The only writes are through the [`spot_storage_contract::Transaction`] the
//! caller passes.

#![warn(missing_docs)]

pub mod apply;
pub mod codes;
pub mod ctx;
pub mod error;
pub mod outcome;
pub mod partition;
pub mod record;
pub mod validate;
pub mod wire;

pub use apply::{apply, apply_quarantining, check_document, stored_version};
pub use ctx::{
    ApplyCtx, ChainAudit, DeliveryNotice, MirrorAll, NoticeSink, OwnAll, OwnerCheck, Ownership,
    QuarantinePort, RecordingAudit, RecordingNoticeSink, RecordingQuarantine, ReplicaAudit,
    ReplicaMarker, SideEffects,
};
pub use error::Reason;
pub use outcome::ApplyOutcome;
pub use partition::{PartitionOwners, ReceivedRecordView, ScopeFields, ScopeSource};
pub use record::{OriginFields, ReceivedRecord, held_version, mark_origin, with_fields};
pub use validate::{validate_document, validate_origin_fields, validate_record};
pub use wire::{ChildPayload, DocPayload, RecordPayload, WireFields, WireValue};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-replica-apply";
