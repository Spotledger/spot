//! `spot-offline-identity`: identity for branch and offline nodes (WP-IAM-09.T1, ADR-070 rule
//! 12.13 b, c, d, h and i).
//!
//! A branch may run the full system without the hub. What it may believe in that time is a
//! **signed permission snapshot** that expires; this crate is that snapshot, its lease, the
//! checks that fail closed when the lease lapses, the node's own identity, and the metadata
//! that lets the hub sort offline writes into accepted, flagged and quarantined.
//!
//! # Shape
//!
//! - [`snapshot`]: the [`Snapshot`] and its canonical bytes and hash.
//! - [`signing`]: TUF-style roles (offline root, online snapshot key, small timestamp message)
//!   over the Ed25519 primitives of `spot-bundle-signing`; the [`Signer`] trait.
//! - [`store`]: the node's [`SnapshotStore`]: rollback, fork, replay, expiry and signature
//!   rules, and persistence of the highest epoch and the remaining lease.
//! - [`lease`]: the lease on the monotonic clock; a reboot restores it, charges the unmeasured
//!   gap and never extends it.
//! - [`authorise`]: fail-closed authorisation per tier, and the audited, bounded override.
//! - [`node`]: the node certificate lifecycle, the hub's node registry and the gate in front
//!   of key release.
//! - [`stamp`]: [`OfflineStamp`] and the hash-chained [`OfflineJournal`].
//! - [`classify`]: the hub's verdicts and the sync into the quarantine queue.
//!
//! # Rules this crate keeps
//!
//! - **Sans-I/O.** Time is an argument: the node's monotonic clock and boot id ([`NodeClock`]),
//!   and the hub's time, which enters only inside a message the hub signed (a snapshot, a
//!   timestamp or a [`HubTime`]). The wall clock is never read and never used for a lease. Persistence, signing, auditing, key release and quarantine
//!   are traits.
//! - **Fail closed.** A missing fact (no snapshot, unknown user, lapsed lease, clock fault,
//!   failed persistence, failed audit) is a denial or a refusal, never an allowance.
//! - **No secrets.** Keys stay behind [`Signer`]; messages, errors and audit records carry
//!   ids, epochs, tiers and counts only.
//! - **Quarantine, not loss.** A write the hub cannot vouch for is held for a person; if it
//!   cannot be held it is not acknowledged and is sent again.

pub mod audit;
pub mod authorise;
mod canonical;
pub mod classify;
pub mod error;
pub mod lease;
pub mod node;
pub mod signing;
pub mod snapshot;
pub mod stamp;
pub mod store;
pub mod types;

pub use audit::{AuditSink, AuditSinkError, MemoryAuditSink};
pub use authorise::{
    Decision, OVERRIDE_PERMISSION, OverrideGrant, OverridePolicy, OverrideRequest,
};
pub use classify::{
    FlagReason, HubView, Outcome, QuarantineItem, QuarantinePort, SyncReport, Verdict, classify,
    sync_journal,
};
pub use error::{DenyReason, OfflineError, QuarantineReason, Rejection};
pub use lease::{LeaseRecord, MAX_CHECKPOINT_GAP};
pub use node::{
    CertLifecycle, CertRecord, GatedKeyRelease, KeyReleasePort, KeyReleaseRequest, MAX_CERT_TTL_MS,
    NodeRegistry, NodeState,
};
pub use signing::{
    Delegation, HubTime, Message, SignedDelegation, SignedHubTime, SignedSnapshot, SignedTimestamp,
    Signer, Timestamp, sign_delegation, sign_hub_time, sign_snapshot, sign_timestamp,
};
pub use snapshot::{
    DirectoryEntry, Grant, NodeRevocationKind, Revocation, Snapshot, SnapshotHash, SyncClass,
    SyncClassEntry, TierDeadlines,
};
pub use stamp::{JournalStore, MemoryJournalStore, OfflineJournal, OfflineStamp, verify_entries};
pub use store::{
    Accepted, AcceptedKind, MemoryPersistence, PersistError, Persisted, Persistence, Received,
    SnapshotStore, TrustAnchor,
};
pub use types::{
    CEILING_MS, FLOOR_MS, HubMs, NodeClock, NodeId, TenantId, Tier, TierBounds, clamp,
};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-offline-identity";
