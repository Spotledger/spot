//! Journal snapshots over PostgreSQL (WP-RTB-01.T5, ADR-061 section 6).
//!
//! At intervals the platform writes a base image of every captured record as of a recorded
//! position `P`, from a consistent read, beside the journal. This crate is the database side:
//!
//! - [`ImageReader`]: one repeatable-read read-only transaction, the outbox head read in it,
//!   keyset pages of `ImageRecord`s sorted by `(entity, id)`, child rows inside their parent's
//!   image, no table locks;
//! - [`take_snapshot`]: reader into `SnapshotWriter`, then **wait for the journal to reach `P`**,
//!   then the signed index last;
//! - [`SnapshotKeyring`]: the signing and verifying keys derived from the site's key provider;
//! - [`retention::plan`]: the last 7 daily and 4 weekly, the newest of each generation and never
//!   the only verified one (a pure plan).
//!
//! The schedule, the after-migration trigger, the conditions and the audit records are the host
//! service's (`spot-cli` `services/journal_snapshots.rs`).
#![forbid(unsafe_code)]

mod error;
mod keyring;
mod read;
pub mod retention;
mod take;

pub use error::SnapshotPgError;
pub use keyring::SnapshotKeyring;
pub use read::{DEFAULT_PAGE_SIZE, Head, ImageReader, scalar};
pub use retention::{Retention, RetentionPlan, SnapshotId};
pub use take::{JournalReach, TakeSettings, take_snapshot};
