//! Continuous verification of a tenant's journal in the object store (WP-JRN-03.T5, ADR-061 s8).
//!
//! One question: for tenant `T` and expected source `S`, is what the store holds a complete,
//! untampered journal? [`verify_tenant`] answers it from the signed manifest, the segment objects
//! and the caller's expectation, and every answer is a list of [`VerifyFinding`]s that name the
//! tenant, the generation and the position range, never object bytes.
//!
//! # Light and deep
//!
//! * [`Mode::Light`] checks the manifest signature and ownership, that every listed segment object
//!   exists, and that its hash (the store's etag, the SHA-256 of the object) is the one the
//!   manifest lists. It reads no segment.
//! * [`Mode::Deep`] reads each segment as well: header source and position against the manifest's
//!   summary, block checksums, and the link to the segment before it.
//!
//! # Ownership
//!
//! The expected tenant, source and generation are arguments, and the keys are the tenant's public
//! keys. A manifest that verifies but names another source, or belongs to another generation than
//! the key it sits under (a stale file, or a copy from another prefix), is
//! [`FindingKind::ForeignManifest`]. A segment whose header names another source is
//! [`FindingKind::ForeignSegment`]. Both are tampering, never "missing". Segment keys are built
//! from the expected tenant, so a segment can only be found under the tenant's own prefix.
//!
//! # Pinned and discovery runs
//!
//! [`verify_pinned`] takes a [`PinnedExpected`]: the generation is mandatory and its manifest is
//! the one verified, with no listing deciding which. A missing pinned manifest is
//! [`FindingKind::MissingManifest`], never a fall back to an older one, and a stored manifest of a
//! higher generation is [`FindingKind::NewerGenerationPresent`] (a stale pin, not tampering).
//! [`verify_tenant`] with `Expected::generation == None` is **discovery only**: it verifies the
//! newest manifest the store lists and so accepts a rollback that removed the newest one;
//! [`VerifyReport::pinned`] says which form produced a report.
//!
//! # Incremental runs and the byte budget
//!
//! A [`VerifyCursor`] remembers the clean prefix of the manifest a pass verified. The next pass
//! (given the cursor in [`VerifyOptions`]) reads only segments after it, as long as the manifest
//! still begins with exactly the listing the cursor covers; if anything already verified changed,
//! the cursor is void, [`FindingKind::ManifestRegressed`] is reported and everything is read
//! again. A pass never reads more than [`VerifyOptions::budget_bytes`] (default
//! [`DEFAULT_BUDGET_BYTES`]): when the budget runs out it returns [`VerifyReport::partial`] with a
//! cursor to continue from.
//!
//! The crate is a library over [`spot_journal_store::JournalStore`]: no clock, no randomness, no
//! async runtime (the functions are plain `async fn`s over the store's boxed futures).

#![forbid(unsafe_code)]

mod cursor;
mod error;
mod finding;
mod report;
mod verify;

#[cfg(test)]
mod tests;

pub use cursor::VerifyCursor;
pub use error::VerifyError;
pub use finding::{FindingKind, VerifyFinding};
pub use report::{
    DEFAULT_BUDGET_BYTES, Expected, Mode, PinnedExpected, VerifyOptions, VerifyReport,
};
pub use verify::{verify_pinned, verify_pinned_with, verify_tenant, verify_tenant_with};
