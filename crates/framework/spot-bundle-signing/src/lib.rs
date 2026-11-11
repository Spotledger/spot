//! Signing for Spotledger app bundles (WS11, WP-STR-02.T1, ADR-21): third-party apps are
//! distributed as signed WASM components.
//!
//! The pieces:
//! - [`keys`]: Ed25519 key pairs (the secret never printed) and key ids (SHA-256 prefix, hex).
//! - [`anchor`]: the store's trust anchor, with root key validity windows for rotation.
//! - [`certificate`]: publisher certificates, signed by a root.
//! - [`signature`]: the detached bundle signature over the canonical digest, bound to app id,
//!   version and publisher.
//! - [`revocation`]: the signed revocation list (keys, publishers, bundle digests).
//! - [`verify`](crate::verify::verify): the one check the installer and the startup loader call.
//! - [`documents`]: the JSON file forms, parsed with every field checked.
//!
//! The crate is pure: no clock, no network, no storage, no floats, no `unsafe`. Time is an
//! argument, and key material is supplied by the caller.

pub mod anchor;
pub mod canonical;
pub mod certificate;
pub mod documents;
pub mod error;
pub mod keys;
pub mod revocation;
pub mod signature;
pub mod verify;

pub use anchor::{TrustAnchor, TrustedRoot};
pub use certificate::{
    CertificateChain, PublisherCertificate, SignedCertificate, issue_certificate,
};
pub use error::SigningError;
pub use keys::{KeyId, KeyPair, PublicKey, generate_keypair};
pub use revocation::{
    RevocationBody, RevocationList, RevokedBundle, RevokedKey, RevokedPublisher,
    issue_revocation_list,
};
pub use signature::{BundleSignature, sign_bundle};
pub use verify::{BundleFacts, Verified, VerifyMode, VerifyPolicy, verify};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bundle-signing";
