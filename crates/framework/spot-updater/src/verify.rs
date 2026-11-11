//! Verification: the signature and trust checks, and the digest checks over the bytes on disk.
//!
//! [`verify_update`] runs in a fixed order and the first failing check decides the refusal, with
//! its own stable `updater.*` code:
//!
//! 1. the update is signed;
//! 2. the revocation list's issuer is a trusted key, valid at `now`, and its signature verifies
//!    (an unknown, not-yet-valid or expired issuer is refused, as is a bad signature);
//! 3. the revocation list is not older than the generation the caller already holds;
//! 4. the signature's key is a trusted key, valid at `now`, and not on the revocation list;
//! 5. the Ed25519 signature verifies over the manifest bytes and the payload digest.
//!
//! [`verify_payload`] and [`verify_file`] then recompute the SHA-256 of the bytes on disk against
//! the manifest, so a flipped byte in the payload or in a single file is located. This module
//! reads no clock and no storage: `now` and the digests are arguments.

use crate::digest::Digest;
use crate::error::UpdaterError;
use crate::keys::KeyId;
use crate::manifest::{UpdateFile, UpdateManifest};
use crate::trust::{TrustedUpdateKey, UpdateRevocation, UpdateSignature, UpdateTrust};

/// What a successful [`verify_update`] establishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedUpdate {
    /// The key that signed the manifest.
    pub key_id: KeyId,
    /// The revocation list generation the check used.
    pub revocation_generation: u64,
}

/// Checks an update's signature and trust. See the module docs for the order.
///
/// # Errors
/// The first failing check's [`UpdaterError`], whose [`UpdaterError::code`] names it.
pub fn verify_update(
    manifest: &UpdateManifest,
    signature: Option<&UpdateSignature>,
    trust: &UpdateTrust,
    revocations: &UpdateRevocation,
    now: i64,
    held_revocation_generation: u64,
) -> Result<VerifiedUpdate, UpdaterError> {
    // 1. The update is signed.
    let signature = signature.ok_or(UpdaterError::Unsigned)?;

    // 2. The revocation list's issuer is trusted and its signature verifies.
    let issuer =
        trust
            .key(&revocations.body.issued_by)
            .ok_or_else(|| UpdaterError::KeyUnknown {
                key_id: revocations.body.issued_by.as_str().to_owned(),
            })?;
    check_window(issuer, now)?;
    issuer
        .public()
        .verify(&revocations.body.signed_bytes(), revocations.signature())
        .map_err(|_| UpdaterError::RevocationSignatureInvalid)?;

    // 3. The list is not older than the one held.
    if revocations.body.generation < held_revocation_generation {
        return Err(UpdaterError::RevocationStale {
            held: held_revocation_generation,
            offered: revocations.body.generation,
        });
    }

    // 4. The signing key is trusted, in its window, and not revoked.
    let key = trust
        .key(signature.key_id())
        .ok_or_else(|| UpdaterError::KeyUnknown {
            key_id: signature.key_id().as_str().to_owned(),
        })?;
    check_window(key, now)?;
    if revocations
        .body
        .keys
        .iter()
        .any(|revoked| &revoked.key_id == signature.key_id())
    {
        return Err(UpdaterError::KeyRevoked {
            key_id: signature.key_id().as_str().to_owned(),
        });
    }

    // 5. The signature, over the bytes recomputed from the manifest.
    key.public()
        .verify(&manifest.signing_message(), signature.signature())?;

    Ok(VerifiedUpdate {
        key_id: signature.key_id().clone(),
        revocation_generation: revocations.body.generation,
    })
}

/// Checks the payload's size and SHA-256 against the manifest.
///
/// # Errors
/// [`UpdaterError::PayloadSizeMismatch`] or [`UpdaterError::PayloadDigestMismatch`].
pub fn verify_payload(
    manifest: &UpdateManifest,
    size: u64,
    digest: Digest,
) -> Result<(), UpdaterError> {
    if size != manifest.payload.size {
        return Err(UpdaterError::PayloadSizeMismatch {
            expected: manifest.payload.size,
            actual: size,
        });
    }
    if digest != manifest.payload.sha256 {
        return Err(UpdaterError::PayloadDigestMismatch {
            expected: manifest.payload.sha256.to_hex(),
            actual: digest.to_hex(),
        });
    }
    Ok(())
}

/// Checks one file's size and SHA-256 against its manifest entry.
///
/// # Errors
/// [`UpdaterError::FileSizeMismatch`] or [`UpdaterError::FileDigestMismatch`].
pub fn verify_file(file: &UpdateFile, size: u64, digest: Digest) -> Result<(), UpdaterError> {
    if size != file.size {
        return Err(UpdaterError::FileSizeMismatch {
            path: file.path.clone(),
            expected: file.size,
            actual: size,
        });
    }
    if digest != file.sha256 {
        return Err(UpdaterError::FileDigestMismatch {
            path: file.path.clone(),
            expected: file.sha256.to_hex(),
            actual: digest.to_hex(),
        });
    }
    Ok(())
}

/// Whether the key's window covers `now`: first failing bound decides.
fn check_window(key: &TrustedUpdateKey, now: i64) -> Result<(), UpdaterError> {
    if now < key.valid_from {
        return Err(UpdaterError::KeyNotYetValid {
            key_id: key.key_id().as_str().to_owned(),
        });
    }
    if key.valid_to.is_some_and(|end| now >= end) {
        return Err(UpdaterError::KeyExpired {
            key_id: key.key_id().as_str().to_owned(),
        });
    }
    Ok(())
}
