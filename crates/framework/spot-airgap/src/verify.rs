//! Verification: the signature, trust and digest checks over an offline bundle.
//!
//! [`verify_bundle`] reuses `spot-bundle-signing`'s one verification: the publisher
//! certificate chains to a trusted root, the root is valid at `now`, the certificate's window
//! covers the signing time, neither the publisher nor the signing key is revoked at that time,
//! the exact bundle digest is not revoked, and the detached Ed25519 signature verifies over the
//! digest recomputed from the caller's manifest. No cryptography is written here.
//!
//! [`verify_file`] then recomputes the SHA-256 of a payload file against the manifest, so a
//! flipped byte anywhere is located — the same size-and-digest rule `spot-updater` applies to
//! update payloads.
//!
//! [`verify_detached`] is the same trust check for a plain detached signature (the result
//! bundle the agent carries out), reusing `spot-updater`'s signature document, key anchor and
//! revocation list. This module reads no clock and no storage: `now` is an argument.

use spot_bundle_signing::{
    BundleFacts, CertificateChain, RevocationList, TrustAnchor, Verified, VerifyMode, VerifyPolicy,
    verify as signing_verify,
};
use spot_updater::{Digest, UpdateRevocation, UpdateSignature, UpdateTrust};

use crate::error::AirgapError;
use crate::manifest::{OfflineFile, OfflineManifest};

/// The trust material an import or an out-of-band verification is checked against.
#[derive(Clone, Copy, Debug)]
pub struct BundleTrust<'a> {
    /// The roots this store trusts.
    pub anchor: &'a TrustAnchor,
    /// The signed revocation list this store holds.
    pub revocations: &'a RevocationList,
    /// The instant the checks run at (UTC seconds); never read from a clock.
    pub now: i64,
    /// The revocation generation the caller already holds; an older list is refused.
    pub held_revocation_generation: u64,
}

/// What a successful [`verify_bundle`] establishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedBundle {
    /// The publisher the certificate names.
    pub publisher_id: String,
    /// The signing key.
    pub key_id: spot_bundle_signing::KeyId,
    /// When the bundle was signed (UTC seconds).
    pub signed_at: i64,
    /// The revocation list generation the check used.
    pub revocation_generation: u64,
    /// The bundle digest the signature covers.
    pub digest: spot_bundle::Digest,
}

/// Checks an offline bundle's signature and trust.
///
/// # Errors
/// The first failing check's [`AirgapError`], whose [`AirgapError::code`] names it.
pub fn verify_bundle(
    manifest: &OfflineManifest,
    signature: &spot_bundle_signing::BundleSignature,
    chain: &CertificateChain,
    trust: &BundleTrust<'_>,
) -> Result<VerifiedBundle, AirgapError> {
    // The stale-list check first, so the caller sees the two generations.
    if trust.revocations.body.generation < trust.held_revocation_generation {
        return Err(AirgapError::RevocationStale {
            held: trust.held_revocation_generation,
            offered: trust.revocations.body.generation,
        });
    }
    let digest = manifest.signing_digest()?;
    let version = manifest.version.to_string();
    let facts = BundleFacts {
        digest: &digest,
        app_id: &manifest.name,
        version: &version,
    };
    let policy = VerifyPolicy {
        mode: VerifyMode::Install,
        allow_expired_certificate_at_startup: false,
        held_revocation_generation: trust.held_revocation_generation,
    };
    let Verified {
        publisher_id,
        key_id,
        signed_at,
        revocation_generation,
    } = signing_verify(
        &facts,
        signature,
        chain,
        trust.anchor,
        trust.revocations,
        trust.now,
        &policy,
    )?;
    Ok(VerifiedBundle {
        publisher_id,
        key_id,
        signed_at,
        revocation_generation,
        digest,
    })
}

/// Checks one payload file's size and SHA-256 against its manifest entry.
///
/// # Errors
/// [`AirgapError::FileSizeMismatch`] or [`AirgapError::FileDigestMismatch`].
pub fn verify_file(file: &OfflineFile, bytes: &[u8]) -> Result<(), AirgapError> {
    let size = bytes.len() as u64;
    if size != file.size {
        return Err(AirgapError::FileSizeMismatch {
            path: file.path.clone(),
            expected: file.size,
            actual: size,
        });
    }
    let digest = Digest::of(bytes);
    if digest != file.sha256 {
        return Err(AirgapError::FileDigestMismatch {
            path: file.path.clone(),
            expected: file.sha256.to_hex(),
            actual: digest.to_hex(),
        });
    }
    Ok(())
}

/// The trust material an agent's result bundle is checked against on the console.
#[derive(Clone, Copy, Debug)]
pub struct AgentTrust<'a> {
    /// The keys the console trusts to have produced result bundles.
    pub keys: &'a UpdateTrust,
    /// The signed revocation list the console holds.
    pub revocations: &'a UpdateRevocation,
    /// The instant the checks run at (UTC seconds).
    pub now: i64,
    /// The revocation generation the caller already holds.
    pub held_revocation_generation: u64,
}

/// What a successful [`verify_detached`] establishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedDetached {
    /// The key that signed.
    pub key_id: spot_updater::KeyId,
    /// The revocation list generation the check used.
    pub revocation_generation: u64,
}

/// Checks a detached Ed25519 signature over `message` against a key anchor and its revocation
/// list. This is `spot-updater`'s verification rule for a plain message: the list's issuer is
/// trusted and its signature verifies, the list is not older than the one held, the signing key
/// is trusted and in its window, not revoked, and the signature verifies.
///
/// # Errors
/// [`AirgapError::ResultUnsigned`], [`AirgapError::RevocationStale`],
/// [`AirgapError::KeyUnknown`], [`AirgapError::KeyNotYetValid`], [`AirgapError::KeyExpired`],
/// [`AirgapError::KeyRevoked`], [`AirgapError::RevocationSignatureInvalid`] or
/// [`AirgapError::ResultSignatureInvalid`].
pub fn verify_detached(
    message: &[u8],
    signature: Option<&UpdateSignature>,
    trust: &AgentTrust<'_>,
) -> Result<VerifiedDetached, AirgapError> {
    let signature = signature.ok_or(AirgapError::ResultUnsigned)?;

    // The revocation list's issuer is trusted and its signature verifies.
    let issuer = trust
        .keys
        .key(&trust.revocations.body.issued_by)
        .ok_or_else(|| AirgapError::KeyUnknown {
            key_id: trust.revocations.body.issued_by.as_str().to_owned(),
        })?;
    check_window(issuer, trust.now)?;
    issuer.public().verify(
        &trust.revocations.body.signed_bytes(),
        trust.revocations.signature(),
    )?;

    // The list is not older than the one held.
    if trust.revocations.body.generation < trust.held_revocation_generation {
        return Err(AirgapError::RevocationStale {
            held: trust.held_revocation_generation,
            offered: trust.revocations.body.generation,
        });
    }

    // The signing key is trusted, in its window, and not revoked.
    let key = trust
        .keys
        .key(signature.key_id())
        .ok_or_else(|| AirgapError::KeyUnknown {
            key_id: signature.key_id().as_str().to_owned(),
        })?;
    check_window(key, trust.now)?;
    if trust
        .revocations
        .body
        .keys
        .iter()
        .any(|revoked| &revoked.key_id == signature.key_id())
    {
        return Err(AirgapError::KeyRevoked);
    }

    // The signature, over the bytes recomputed from the caller.
    key.public().verify(message, signature.signature())?;

    Ok(VerifiedDetached {
        key_id: signature.key_id().clone(),
        revocation_generation: trust.revocations.body.generation,
    })
}

/// Whether a trusted key's window covers `now`; the first failing bound decides.
fn check_window(key: &spot_updater::TrustedUpdateKey, now: i64) -> Result<(), AirgapError> {
    if now < key.valid_from {
        return Err(AirgapError::KeyNotYetValid {
            key_id: key.key_id().as_str().to_owned(),
        });
    }
    if key.valid_to.is_some_and(|end| now >= end) {
        return Err(AirgapError::KeyExpired {
            key_id: key.key_id().as_str().to_owned(),
        });
    }
    Ok(())
}
