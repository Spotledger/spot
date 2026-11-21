//! Sync node identity (WP-RTB-02.T3, ADR-062 section 3).
//!
//! Every sync connection is outbound from the edge, over TLS with node identity. The hub
//! authenticates the edge from its client certificate through a `NodeDirectory`; this crate is
//! the production directory and the lifecycle behind it.
//!
//! # Decisions (lead, 31 Oct 2026)
//!
//! 1. **Pinned self-signed certificates, no CA.** Each node generates a key pair and a
//!    self-signed certificate on first start ([`NodeIdentity`]; the key never leaves the node);
//!    the hub pins the certificate's SHA-256 fingerprint.
//! 2. **Enrolment by a one-time code** ([`EnrolService::create_enrolment`]): 16 random bytes
//!    from an injected source, stored only as a salted hash with its expiry (24 hours by
//!    default), compared in constant time, single use enforced by a row lock (of concurrent
//!    redemptions exactly one wins). Unknown, expired and used codes are the **same** error.
//! 3. **Rotation with overlap** ([`EnrolService::rotate`]): a new certificate signed (proof of
//!    possession) by the current key; both fingerprints are valid for the overlap (7 days by
//!    default, 30 at most), then the old one is dropped. [`EnrolService::expiring`] feeds the
//!    `CERT_EXPIRING` condition (amber inside 30 days of `not_after`, red inside 3).
//! 4. **Revocation is immediate**: a revoked or suspended node's next request is refused before
//!    any handler ([`DurableDirectory::mark`]); a revoked fingerprint is never pinned again.
//!
//! # Pieces
//!
//! | Piece | What |
//! | --- | --- |
//! | [`EnrolmentStore`], [`MemoryEnrolmentStore`], [`PgEnrolmentStore`] | the store seam; every change commits with its `sys_sync_node_admin` audit record |
//! | [`DurableDirectory`] | the `NodeDirectory`: a refreshed in-memory snapshot, fail-closed when stale |
//! | [`EnrolService`] | create, redeem, rotate, suspend, resume, revoke, list, expiring |
//! | [`NodeIdentity`], [`ProofOfPossession`] | the node side |
//!
//! # Security
//!
//! Tenant isolation first: each store and directory is one tenant's, every statement filters by
//! it ([`pg::STATEMENTS`]) and a certificate pinned in two tenants authenticates only where it is
//! enrolled. Codes are never logged and never in an audit record (only the enrolment id is). A
//! node cannot re-enrol over an active identity (`sync_enrol.node_exists`). The private key
//! appears in no log, error, `Debug` form or audit record. Time is an argument everywhere; clock
//! skew is tolerated to five minutes and no more.
//!
//! The PostgreSQL store is synchronous over the shared blocking bridge (ADR-082): use it from the
//! hub's control-plane process, never from an async task.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cert;
pub mod code;
pub mod directory;
pub mod error;
pub mod memory;
pub mod model;
pub mod node;
pub mod pg;
pub mod service;
pub mod store;

pub use cert::{ParsedCert, SKEW_SECONDS, fingerprint_of};
pub use code::RandomSource;
pub use directory::{Clock, DirectoryConfig, DurableDirectory};
pub use error::{EnrolError, Result};
pub use memory::MemoryEnrolmentStore;
pub use model::{
    AuditEvent, CertState, ExpiringCert, NewEnrolment, NodeStatus, NodeSummary, PinnedCert,
    RedeemRequest, Redeemed, Rotation, Severity,
};
pub use node::{NodeIdentity, ProofOfPossession};
pub use pg::PgEnrolmentStore;
pub use service::{EnrolRequest, EnrolService, Ids, IssuedEnrolment};
pub use store::EnrolmentStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-enrol";
