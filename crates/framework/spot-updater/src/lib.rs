//! The update manifest and its verification (WP-PRD-03.T1, part 1).
//!
//! An update is a signed manifest (`update.toml`) and a detached signature (`update.sig`). The
//! manifest names the product, the channel, the version, the payload archive with its size and
//! SHA-256, every file inside it, the schema epoch the data layer compares, and the oldest
//! installed version that may update directly ([`UpdateManifest::min_from`]). This crate proves
//! three things about an offered update, all as pure functions with no clock, no network and no
//! filesystem:
//!
//! 1. **The bytes are canonical.** [`UpdateManifest::canonical_bytes`] is one exact byte
//!    sequence per manifest (sorted keys, no whitespace, integer-only), so a signature over it
//!    means one thing. The signature covers the canonical manifest bytes plus the payload
//!    digest ([`UpdateManifest::signing_message`]).
//! 2. **The signature is trusted.** [`verify_update`] checks the manifest signature and the
//!    signed revocation list against the update trust anchor ([`UpdateTrust`]): its own keys,
//!    with validity windows and a strictly increasing revocation generation. The update keys
//!    are their own anchor — never the store's and never the licence's — so a compromised app
//!    publisher key can never sign a platform update.
//! 3. **The bytes on disk are what the manifest claims.** [`verify_payload`] and [`verify_file`]
//!    recompute the SHA-256 of the payload and of one file against the manifest, so a flipped
//!    byte in the archive, the manifest or a single file is located.
//!
//! On top of verification, [`decide`] is the pure policy: **an older version than installed is
//! refused** ([`UpdaterErrorCode::Downgrade`]), a different product or a channel the policy does
//! not allow is refused, a payload needing a newer schema epoch than the database supports is
//! refused unless the manifest lists the migration, and an installed version older than
//! `min_from` steps through a listed `via`. A signed `allow_downgrade` is not supported: rollback
//! (a later task) is the supported way back.
//!
//! The staging layout, the switch-over, rollback, crash-resume and the filesystem and service
//! traits are the second half of the update layer (WP-PRD-03.T3): [`UpdateEngine`] runs the
//! steps over the [`FileSystem`], [`ServiceControl`] and [`DataHooks`] seams, rolls back on any
//! failure, and resumes a crashed run from its [`Journal`]. The in-memory fakes with failure and
//! power-cut injection live in `testing` behind the `testing` feature.
//!
//! No `unsafe`, no floats, no clock reads (time is an argument), no storage.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod archive;
pub mod canonical;
pub mod decide;
pub mod digest;
pub mod engine;
pub mod error;
pub mod fs;
pub mod hooks;
pub mod journal;
pub mod keys;
pub mod layout;
pub mod manifest;
pub mod service;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod trust;
pub mod verify;

#[cfg(test)]
mod tests;

pub use archive::{ArchiveLimits, StagedFile, normalise, verify_archive};
pub use canonical::{UPDATE_DOMAIN, UPDATE_REVOCATION_DOMAIN};
pub use decide::{Decision, Installed, Offered, UpdatePolicy, decide};
pub use digest::Digest;
pub use engine::{
    ApplyRequest, Outcome, UpdateEngine, required_free_space, requires_schema_change,
};
pub use error::{UpdaterError, UpdaterErrorCode};
pub use fs::{FileSystem, StdFileSystem};
pub use hooks::{DataHooks, NoDataHooks};
pub use journal::{JOURNAL_SCHEMA, Journal, Stage, Step};
pub use keys::{KeyId, PUBLIC_KEY_BYTES, PublicKey, SIGNATURE_BYTES};
pub use layout::Layout;
pub use manifest::{Channel, MANIFEST_FORMAT, UpdateFile, UpdateManifest, UpdatePayload};
pub use service::ServiceControl;
pub use spot_core::Clock;
pub use trust::{
    RevokedUpdateKey, SIGNATURE_FORMAT, SigningKey, TrustedUpdateKey, UpdateRevocation,
    UpdateRevocationBody, UpdateSignature, UpdateTrust,
};
pub use verify::{VerifiedUpdate, verify_file, verify_payload, verify_update};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-updater";
