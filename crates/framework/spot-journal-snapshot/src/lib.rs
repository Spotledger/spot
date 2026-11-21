//! Journal snapshots as library code (WP-JRN-05.T2, ADR-061 section 6).
//!
//! A snapshot is a base image of every captured record as of one position. Given a consistent,
//! sorted iterator of [`ImageRecord`]s (the database read is a later task), [`SnapshotWriter`]
//! writes them under `<tenant>/snapshots/<generation>/<position>/` as zstd parts (one JSON line
//! per image, a SHA-256 per part in the index) and writes the signed `index.json` **last**, so a
//! snapshot without an index is incomplete by construction and nothing lists it.
//! [`SnapshotReader`] verifies the signature and every part hash while streaming. [`list_snapshots`]
//! gives the rebuild planner its [`SnapshotInfo`] values, and [`mark_verified`] is how the rebuild
//! test (WP-JRN-07.T2) makes one trusted.
//!
//! # Guarantees
//!
//! * Determinism: identical input and parameters produce byte-identical objects, so two nodes
//!   writing the same snapshot agree, and a rerun after a crash converges (an identical existing
//!   object is accepted, a different one is [`SnapshotError::Exists`]).
//! * Tenancy: every key is built from the tenant the writer or reader was given; the signature
//!   covers tenant, generation and position, so an index cannot be moved to another prefix.
//! * No clock, randomness, key material or network: time, keys and the signer are arguments.
//!
//! # Not here
//!
//! The database snapshot read, scheduling, retention, and encryption of parts (the
//! [`PartTransform`] seam, identity by default, is where WP-JRN-08.T4 plugs in).
//!
//! The futures are plain `std` futures over the runtime-free store trait: any executor drives them.

#![forbid(unsafe_code)]

mod error;
mod index;
mod list;
mod reader;
mod record;
mod sign;
mod writer;

#[cfg(test)]
mod tests;

pub use error::SnapshotError;
pub use index::{FORMAT, INDEX_NAME, IndexBody, PartEntry, SignedIndex, VERIFIED_NAME};
pub use list::{list_snapshots, mark_verified};
pub use reader::{ImageStream, SnapshotReader, verify_deep};
pub use record::ImageRecord;
pub use sign::{IdentityTransform, PartTransform, SnapshotKeys, SnapshotSigner};
pub use spot_journal_rebuild_plan::SnapshotInfo;
pub use writer::{DEFAULT_PART_BYTES, SnapshotParams, SnapshotWriter};
