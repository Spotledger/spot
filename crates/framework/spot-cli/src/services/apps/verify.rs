//! Start-up verification of an installed bundle: digest, signature and the revocation list.
//!
//! The same checks the installer made run again every time the site starts (WP-STR-02 acceptance):
//! a bundle whose bytes changed since the install no longer has the registry's digest, and a
//! publisher or key revoked since then is on the site's revocation list. Either way the app is not
//! loaded; the refusal carries a stable `app.*` code and never a key, a signature or a payload.

use std::path::Path;

use spot_bundle_signing::{
    BundleSignature, CertificateChain, PublicKey, RevocationList, SignedCertificate, SigningError,
    TrustAnchor, TrustedRoot, VerifyMode, VerifyPolicy,
};
use spot_manifest::AppManifest;
use spot_pack::{PackOptions, verify_packed};

use super::store::StoredBundle;

/// `app.bundle_invalid`: the bundle does not open, has another digest than the registry's, or its
/// signature does not verify.
pub const BUNDLE_INVALID: &str = "app.bundle_invalid";
/// `app.publisher_revoked`: the publisher, the signing key or the bundle was revoked.
pub const PUBLISHER_REVOKED: &str = "app.publisher_revoked";

/// Why an app was not loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The stable code.
    pub code: &'static str,
    /// What happened, in plain words.
    pub message: String,
}

impl Refusal {
    fn new(code: &'static str, message: &str) -> Self {
        Self {
            code,
            message: message.to_owned(),
        }
    }
}

/// The site's trust settings, loaded once.
pub struct Trust {
    anchor: Option<TrustAnchor>,
    revocations: Option<RevocationList>,
    /// Whether a bundle without a signature is refused (`[trust] require_signed`).
    pub require_signed: bool,
}

impl Trust {
    /// Trust that holds nothing: every signed bundle is untrusted, and an unsigned one is
    /// refused when `require_signed` is true.
    #[must_use]
    pub fn none(require_signed: bool) -> Self {
        Self {
            anchor: None,
            revocations: None,
            require_signed,
        }
    }

    /// Trust from an anchor and a revocation list already in hand.
    #[must_use]
    pub fn new(anchor: TrustAnchor, revocations: RevocationList, require_signed: bool) -> Self {
        Self {
            anchor: Some(anchor),
            revocations: Some(revocations),
            require_signed,
        }
    }

    /// Reads the roots file (one base64 public key per line) and the revocation list, as the
    /// installer does. A file that is missing or unreadable leaves that part empty, which trusts
    /// nothing.
    #[must_use]
    pub fn load(
        root: &Path,
        roots: Option<&Path>,
        revocations: Option<&Path>,
        require_signed: bool,
    ) -> Self {
        let anchor = roots.and_then(|p| read_anchor(&root.join(p)));
        let revocations = revocations
            .and_then(|p| std::fs::read(root.join(p)).ok())
            .and_then(|bytes| RevocationList::from_json(&bytes).ok());
        Self {
            anchor,
            revocations,
            require_signed,
        }
    }
}

fn read_anchor(path: &Path) -> Option<TrustAnchor> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut roots = Vec::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let key = spot_bundle_signing::canonical::unb64(line)
            .and_then(|b| PublicKey::from_bytes(&b).ok())?;
        roots.push(TrustedRoot::new(key, 0, None));
    }
    (!roots.is_empty()).then(|| TrustAnchor::new(roots))
}

/// What a verified bundle gives the runtime.
pub struct Verified {
    /// The manifest, as signed.
    pub manifest: AppManifest,
    /// The component binary; empty for an app with no WebAssembly component (a native app).
    pub wasm: Vec<u8>,
    /// The canonical digest, 64 hex digits.
    pub digest_hex: String,
    /// The publisher the certificate names; `None` for a development, unsigned bundle.
    pub publisher: Option<String>,
}

fn map_signing(error: &SigningError) -> Refusal {
    use SigningError as E;
    match error {
        E::PublisherRevoked | E::KeyRevoked | E::KeyCompromised | E::BundleRevoked => Refusal::new(
            PUBLISHER_REVOKED,
            "the publisher, the signing key or this bundle was revoked",
        ),
        _ => Refusal::new(
            BUNDLE_INVALID,
            "the bundle's signature or publisher certificate does not verify",
        ),
    }
}

/// Opens `stored`, checks it has the digest the registry holds, and verifies its signature at
/// `now` (UTC seconds, from the site clock) in start-up mode.
///
/// # Errors
///
/// A [`Refusal`]: `app.bundle_invalid` or `app.publisher_revoked`.
pub fn verify(
    stored: &StoredBundle,
    expected_digest_hex: &str,
    trust: &Trust,
    now: i64,
) -> Result<Verified, Refusal> {
    let options = PackOptions::default();
    // `open` never reads the digest field; a parsed all-zero value fills it without naming the
    // bundle crate's type (the installer does the same).
    let Ok(unused) = "0".repeat(64).parse() else {
        return Err(Refusal::new(BUNDLE_INVALID, "the bundle cannot be opened"));
    };
    let packed = spot_pack::PackedBundle {
        bytes: stored.bytes.clone(),
        digest: unused,
    };
    let bundle = packed
        .open(&options)
        .map_err(|_| Refusal::new(BUNDLE_INVALID, "the bundle does not open as a bundle"))?;
    let digest_hex = bundle.digest().to_hex();
    if digest_hex != expected_digest_hex {
        return Err(Refusal::new(
            BUNDLE_INVALID,
            "the bundle's digest is not the one the app was installed with",
        ));
    }

    let publisher = if stored.signature.is_empty() {
        if trust.require_signed {
            return Err(Refusal::new(
                BUNDLE_INVALID,
                "the bundle has no signature and this site requires signed apps",
            ));
        }
        None
    } else {
        let signature = BundleSignature::from_json(&stored.signature)
            .map_err(|_| Refusal::new(BUNDLE_INVALID, "the signature file cannot be read"))?;
        let certificate = SignedCertificate::from_json(&stored.certificate).map_err(|_| {
            Refusal::new(BUNDLE_INVALID, "the publisher certificate cannot be read")
        })?;
        let (Some(anchor), Some(revocations)) = (&trust.anchor, &trust.revocations) else {
            return Err(Refusal::new(
                BUNDLE_INVALID,
                "this site has no trust roots or no revocation list to verify against",
            ));
        };
        let policy = VerifyPolicy {
            mode: VerifyMode::Startup,
            allow_expired_certificate_at_startup: true,
            held_revocation_generation: 0,
        };
        let proof = verify_packed(
            &bundle,
            &signature,
            &CertificateChain::new(certificate),
            anchor,
            revocations,
            now,
            &policy,
        )
        .map_err(|refusal| map_signing(&refusal.0))?;
        Some(proof.publisher_id)
    };
    Ok(Verified {
        manifest: bundle.manifest().clone(),
        wasm: bundle.wasm().to_vec(),
        digest_hex,
        publisher,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_that_are_not_a_bundle_are_invalid() {
        let stored = StoredBundle {
            bytes: b"not a zip".to_vec(),
            ..StoredBundle::default()
        };
        let refusal = verify(&stored, &"0".repeat(64), &Trust::none(false), 0)
            .err()
            .map(|r| r.code);
        assert_eq!(refusal, Some(BUNDLE_INVALID));
    }

    #[test]
    fn revocation_errors_map_to_the_revoked_code() {
        for e in [
            SigningError::PublisherRevoked,
            SigningError::KeyRevoked,
            SigningError::KeyCompromised,
            SigningError::BundleRevoked,
        ] {
            assert_eq!(map_signing(&e).code, PUBLISHER_REVOKED);
        }
        assert_eq!(
            map_signing(&SigningError::SignatureInvalid).code,
            BUNDLE_INVALID
        );
    }
}
