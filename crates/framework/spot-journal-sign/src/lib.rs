//! Ed25519 adapters for the journal snapshot (WP-JRN-05.T4, ADR-061 s5 and s8).
//!
//! `spot-journal-snapshot` signs its index and its verified marker through two traits and holds no
//! key. This crate fills them with the tenant journal key:
//!
//! - [`Ed25519SnapshotSigner`] signs with a key the caller supplies (the crate generates no key
//!   and reads no randomness; Ed25519 signatures are deterministic).
//! - [`ManifestSnapshotKeys`] verifies through the manifest's key windows
//!   ([`spot_journal_manifest::VerifyingKeys::verify_message`]), so a snapshot follows the same
//!   rotation rule as a manifest: an old key stays trusted for the generations it signed and no
//!   others.
//!
//! The tenant is bound by the key set the caller builds from that tenant's public keys: a key set
//! of another tenant does not verify this tenant's signatures. Key storage and rotation are out of
//! scope.
//!
//! No key material appears in `Debug` output, and the signer is not `Clone`. The crate returns no
//! error of its own (verification answers `bool`, as the trait requires), so it has no error
//! catalogue.

mod keys;
mod signer;

pub use keys::ManifestSnapshotKeys;
pub use signer::Ed25519SnapshotSigner;

#[cfg(test)]
mod tests;
