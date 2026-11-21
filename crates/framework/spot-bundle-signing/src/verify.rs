//! Verification: the one function the installer (WP-STR-04) and the startup check both call.
//!
//! The checks run in a fixed order and the first failing one decides the refusal, with its own
//! stable `signing.*` code:
//!
//! 1. the signing certificate chains to a trusted root, and the root is valid at `now`;
//! 2. the certificate's signature verifies under that root, and the revocation list's signature
//!    verifies under its issuing root;
//! 3. the certificate's window covers `now` (see [`VerifyMode`] for the expired case), and the
//!    signature's `signed_at` lies inside the certificate's window, in both modes;
//! 4. the signature names the certificate's publisher and key, and neither is revoked at the
//!    signature's time (a compromised key is refused whenever it signed);
//! 5. the bundle's digest is not on the revocation list;
//! 6. the Ed25519 signature verifies over the digest recomputed from the caller's bundle, the
//!    app id, the version, the publisher, the key id and the signing time;
//! 7. the app id and version the signature names are the manifest's;
//! 8. the revocation list is not older than the generation the caller already holds.
//!
//! This function reads no clock and no storage: `now` and the lists are arguments.

use spot_bundle::Digest;

use crate::anchor::TrustAnchor;
use crate::certificate::CertificateChain;
use crate::error::SigningError;
use crate::keys::KeyId;
use crate::revocation::RevocationList;
use crate::signature::{BundleSignature, signed_bytes};

/// Where a signature is checked, which decides what an expired certificate means.
///
/// In both modes the signature's `signed_at` must lie inside the certificate's validity window
/// `[valid_from, valid_to)`: a signer chooses `signed_at`, so a certificate that expired cannot
/// be used to sign a new bundle with a time inside its old window, and a time outside the window
/// is refused whatever `now` is.
///
/// - [`VerifyMode::Install`]: an installer. An expired certificate is always refused.
/// - [`VerifyMode::Startup`]: the startup check of an app already installed. An expired
///   certificate is refused unless [`VerifyPolicy::allow_expired_certificate_at_startup`] is set,
///   so a publisher's certificate running out does not switch off apps that were checked at
///   install time. An accepted expired certificate still needs a `signed_at` inside its window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyMode {
    /// Installing an app.
    Install,
    /// Checking an installed app at startup.
    Startup,
}

/// How a check is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifyPolicy {
    /// Where the check runs.
    pub mode: VerifyMode,
    /// At startup only: accept a certificate that has expired, when the signature was made
    /// while it was valid. Ignored at install.
    pub allow_expired_certificate_at_startup: bool,
    /// The revocation generation the caller already holds; an older list is refused.
    pub held_revocation_generation: u64,
}

/// The bundle as the caller holds it: its canonical digest and the manifest's identity.
#[derive(Clone, Copy, Debug)]
pub struct BundleFacts<'a> {
    /// The bundle's canonical digest, from [`spot_bundle::Bundle::digest`].
    pub digest: &'a Digest,
    /// The manifest's app name.
    pub app_id: &'a str,
    /// The manifest's version.
    pub version: &'a str,
}

/// What a successful check establishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The publisher the certificate names.
    pub publisher_id: String,
    /// The signing key.
    pub key_id: KeyId,
    /// When it was signed (UTC seconds).
    pub signed_at: i64,
    /// The revocation list generation the check used.
    pub revocation_generation: u64,
}

/// Checks a bundle's signature. See the module docs for the order.
///
/// # Errors
/// The first failing check's [`SigningError`], whose [`SigningError::code`] names it.
pub fn verify(
    bundle: &BundleFacts<'_>,
    signature: &BundleSignature,
    chain: &CertificateChain,
    anchor: &TrustAnchor,
    revocations: &RevocationList,
    now: i64,
    policy: &VerifyPolicy,
) -> Result<Verified, SigningError> {
    let certificate = &chain.leaf;
    let body = &certificate.body;

    // 1-2. Chains: the certificate's root, and the revocation list's root.
    let root = anchor
        .root(&body.issued_by)
        .ok_or(SigningError::UnknownRoot)?;
    if !root.valid_at(now) {
        return Err(SigningError::RootOutsideWindow);
    }
    root.public()
        .verify(&body.signed_bytes(), certificate.signature())
        .map_err(|_| SigningError::CertificateSignatureInvalid)?;
    let list_root = anchor
        .root(&revocations.body.issued_by)
        .ok_or(SigningError::UnknownRoot)?;
    if !list_root.valid_at(now) {
        return Err(SigningError::RootOutsideWindow);
    }
    list_root
        .public()
        .verify(&revocations.body.signed_bytes(), revocations.signature())
        .map_err(|_| SigningError::RevocationSignatureInvalid)?;

    // 3. The certificate's window.
    if now < body.valid_from {
        return Err(SigningError::CertificateNotYetValid);
    }
    if now >= body.valid_to {
        let startup_allowed =
            policy.mode == VerifyMode::Startup && policy.allow_expired_certificate_at_startup;
        if !startup_allowed {
            return Err(SigningError::CertificateExpired);
        }
    }
    // The signing time, which the signer chose, must be inside the window in both modes.
    if signature.signed_at < body.valid_from || signature.signed_at >= body.valid_to {
        return Err(SigningError::SignedOutsideCertificateWindow);
    }

    // 4. The identities the signature names, and revocation at its time.
    if signature.publisher_id != body.publisher_id {
        return Err(SigningError::PublisherMismatch);
    }
    if signature.key_id != body.key.key_id() {
        return Err(SigningError::SignatureKeyMismatch);
    }
    let publisher_revoked =
        revocations.body.publishers.iter().any(|p| {
            p.publisher_id == body.publisher_id && p.effective_from <= signature.signed_at
        });
    if publisher_revoked {
        return Err(SigningError::PublisherRevoked);
    }
    for key in revocations
        .body
        .keys
        .iter()
        .filter(|k| k.key_id == signature.key_id)
    {
        if key.compromised {
            return Err(SigningError::KeyCompromised);
        }
        if key.effective_from <= signature.signed_at {
            return Err(SigningError::KeyRevoked);
        }
    }

    // 5. The exact digest is not revoked.
    let digest_hex = bundle.digest.to_hex();
    if revocations
        .body
        .bundles
        .iter()
        .any(|b| b.digest_hex == digest_hex)
    {
        return Err(SigningError::BundleRevoked);
    }

    // 6. The signature, over the bytes recomputed from the caller's bundle.
    let message = signed_bytes(
        bundle.digest,
        &signature.app_id,
        &signature.app_version,
        &signature.publisher_id,
        &signature.key_id,
        signature.signed_at,
    );
    body.key
        .verify(&message, signature.signature())
        .map_err(|_| SigningError::SignatureInvalid)?;

    // 7. The app and version are the manifest's.
    if signature.app_id != bundle.app_id {
        return Err(SigningError::AppMismatch);
    }
    if signature.app_version != bundle.version {
        return Err(SigningError::VersionMismatch);
    }

    // 8. The revocation list is not older than the one held.
    if revocations.body.generation < policy.held_revocation_generation {
        return Err(SigningError::StaleRevocationList);
    }

    Ok(Verified {
        publisher_id: body.publisher_id.clone(),
        key_id: signature.key_id.clone(),
        signed_at: signature.signed_at,
        revocation_generation: revocations.body.generation,
    })
}
