//! spot-airgap — air-gapped operation (WP-ADM-12.T1, blueprint WS22, ADR-44).
//!
//! A server with no network access to the outside must still be installed and upgraded, and
//! administrators must still manage it. Everything crosses the gap as a **signed, verifiable
//! file** on removable media:
//!
//! - [`manifest`] — the [`OfflineManifest`] (`offline.toml`): a deterministic description of a
//!   bundle (`runtime`, `agent`, `pack` or `os-mirror`) with every file's size and SHA-256 and
//!   a canonical bundle digest. Its shape mirrors `spot-updater`'s manifest rules.
//! - [`bundle`] — [`OfflineBundle`]: the manifest, its detached signature and the publisher
//!   certificate read from media together, with one verification call.
//! - [`verify`] — the signature, certificate-chain and revocation checks, delegated to
//!   `spot-bundle-signing`; the file size-and-digest checks; and the detached-signature rule
//!   for the agent's result bundle. No cryptography is written here.
//! - [`import`] — [`import`](import::import): verify a bundle from media, refuse a downgrade or
//!   a changed digest, stage it, rename the staging directory into the repository and index it
//!   atomically. An interrupted import is resumable.
//! - [`repo`] — the local repository layout and its `index.toml`.
//! - [`mirror`] — local OS package mirror support: a manifest of package files, verification of
//!   the files, and the generated-metadata step behind the [`MirrorBuilder`](mirror::MirrorBuilder)
//!   trait. The crate verifies; it never downloads.
//! - [`command_bundle`] — [`CommandBundle`](command_bundle::CommandBundle): signed administrator
//!   commands carried in for an air-gapped agent, verified by the agent's own
//!   `spot-signed-command` verifier and nonce window.
//! - [`result`] — [`ResultBundle`](result::ResultBundle): the agent's results and audit-chain
//!   segment, signed by the agent, verified and recorded idempotently on the console.
//! - [`export`] — [`BundleBuilder`](export::BundleBuilder): assembling a bundle on a connected
//!   machine, with the signing keys kept outside this crate behind the
//!   [`Signer`](export::Signer) callback.
//!
//! # Purity
//!
//! No network, no clock reads (every instant is an argument), no floats, no `unsafe`, and no
//! `unwrap`/`expect` outside tests. The one external command the crate composes (repository
//! metadata generation) runs through the [`CommandRunner`](mirror::CommandRunner) seam, so a
//! test never shells out. Secrets never enter this crate: signing keys stay behind the
//! [`Signer`](export::Signer) callback and the agent's key behind `spot-signed-command`'s
//! [`Signer`](spot_signed_command::Signer) trait.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bundle;
pub mod command_bundle;
pub mod error;
pub mod export;
pub mod import;
pub mod manifest;
pub mod mirror;
pub mod repo;
pub mod result;
pub mod storage;
pub mod verify;

pub use bundle::OfflineBundle;
pub use command_bundle::{BundledCommand, BundledSignature, CommandBundle};
pub use error::AirgapError;
pub use export::{BundleBuilder, BundleSpec, Signer};
pub use import::{ImportReport, ImportRequest, import};
pub use manifest::{
    BUNDLE_DIGEST_DOMAIN, BundleKind, MEDIA_CERTIFICATE, MEDIA_FILES_DIR, MEDIA_MANIFEST,
    MEDIA_SIGNATURE, OFFLINE_MANIFEST_FORMAT, OfflineFile, OfflineManifest,
};
pub use mirror::{
    AptFtpArchive, CommandRunner, CreaterepoC, MirrorBuilder, MirrorManifest, MirrorPackage,
    MirrorReport, StdCommandRunner, build_metadata, verify_mirror,
};
pub use repo::{LocalRepo, RepoEntry, RepoIndex};
pub use result::{
    AuditSegment, CommandResult, ResultBundle, ResultImport, ResultOutcome, ResultSink,
    import_result, sign_result,
};
pub use storage::{StdStorage, Storage, walk_files};
pub use verify::{
    AgentTrust, BundleTrust, VerifiedBundle, VerifiedDetached, verify_bundle, verify_detached,
    verify_file,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-airgap";
