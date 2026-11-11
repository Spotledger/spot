//! Signing a packed app with the publisher's key, which lives outside this crate.
//!
//! The [`Signer`] seam is the only way key material enters. A signer returns the detached
//! signature over the bundle's canonical digest plus the certificate chain and the trust
//! material the verifier should use. [`sign`] then runs the **same** `spot-bundle-signing`
//! verifier the installer and startup check call, so a package is never handed on with a
//! signature that would later be refused.
//!
//! The signature binds the app id, the version and the publisher to the digest. Repacking the
//! same content verifies; one changed byte does not. The publisher id and licence are only
//! references inside the bundle; the certificate is the authority.

use spot_bundle::Digest;
use spot_bundle_signing::{
    BundleFacts, BundleSignature, CertificateChain, RevocationList, SigningError, TrustAnchor,
    VerifyMode, VerifyPolicy, verify,
};

use crate::error::Error;
use crate::package::BundleSource;

/// Everything a publisher's signer returns for one bundle.
#[derive(Clone, Debug)]
pub struct SigningMaterial {
    /// The detached signature over the canonical digest.
    pub signature: BundleSignature,
    /// The publisher's certificate chain.
    pub chain: CertificateChain,
    /// The store roots the verifier trusts.
    pub anchor: TrustAnchor,
    /// The revocation list the verifier holds.
    pub revocations: RevocationList,
}

/// Holds the publisher key and signs a bundle's canonical digest.
///
/// Implementations keep the private key outside this crate (a hardware token, an HSM or the
/// publisher's own process). The crate never sees it and never logs it.
pub trait Signer {
    /// The publisher id the key belongs to.
    fn publisher_id(&self) -> &str;

    /// Signs `digest` for `app_id` at `app_version`, at `signed_at` (UTC seconds).
    ///
    /// # Errors
    /// A plain sentence when the key cannot sign (locked, missing, wrong identity).
    fn sign(
        &self,
        digest: &Digest,
        app_id: &str,
        app_version: &str,
        signed_at: i64,
    ) -> Result<SigningMaterial, String>;
}

/// A signed no-code app, ready to publish.
#[derive(Clone, Debug)]
pub struct SignedSource {
    /// The packed and validated source.
    source: BundleSource,
    /// The publisher signature.
    signature: BundleSignature,
    /// The certificate chain the signature names.
    chain: CertificateChain,
    /// The publisher id the verifier established.
    publisher: String,
}

impl SignedSource {
    /// The packed source.
    #[must_use]
    pub fn source(&self) -> &BundleSource {
        &self.source
    }

    /// The `.spotapp` bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.source.bytes()
    }

    /// The canonical digest the signature covers.
    #[must_use]
    pub fn digest(&self) -> Digest {
        self.source.digest()
    }

    /// The detached signature.
    #[must_use]
    pub fn signature(&self) -> &BundleSignature {
        &self.signature
    }

    /// The certificate chain.
    #[must_use]
    pub fn chain(&self) -> &CertificateChain {
        &self.chain
    }

    /// The verified publisher id.
    #[must_use]
    pub fn publisher(&self) -> &str {
        &self.publisher
    }

    /// The app id.
    #[must_use]
    pub fn app(&self) -> &str {
        self.source.manifest().app.name.as_str()
    }

    /// The app version.
    #[must_use]
    pub fn version(&self) -> &semver::Version {
        &self.source.manifest().app.version
    }
}

/// Signs `source` and verifies the result before returning it.
///
/// # Errors
/// [`Error::Signature`] when the signer's material names another identity, when the signature
/// does not verify under the supplied trust material, or when it is outside the certificate's
/// validity window at `signed_at`.
pub fn sign(
    source: BundleSource,
    signer: &dyn Signer,
    signed_at: i64,
) -> Result<SignedSource, Error> {
    let app_id = source.manifest().app.name.as_str().to_owned();
    let version = source.manifest().app.version.to_string();
    let digest = source.digest();
    let material = signer
        .sign(&digest, &app_id, &version, signed_at)
        .map_err(|message| Error::Signature { message })?;

    if material.signature.app_id != app_id || material.signature.app_version != version {
        return Err(Error::Signature {
            message: format!(
                "the signer signed `{}` {} but the bundle is `{app_id}` {version}",
                material.signature.app_id, material.signature.app_version
            ),
        });
    }
    if material.signature.publisher_id != signer.publisher_id() {
        return Err(Error::Signature {
            message: "the signature names a publisher other than the signer".to_owned(),
        });
    }
    if material.signature.publisher_id != source.publisher() {
        return Err(Error::Signature {
            message: format!(
                "the certificate names `{}` but the app is published by `{}`",
                material.signature.publisher_id,
                source.publisher()
            ),
        });
    }

    let facts = BundleFacts {
        digest: &digest,
        app_id: &app_id,
        version: &version,
    };
    let policy = VerifyPolicy {
        mode: VerifyMode::Install,
        allow_expired_certificate_at_startup: false,
        held_revocation_generation: material.revocations.body.generation,
    };
    let verified: spot_bundle_signing::Verified = verify(
        &facts,
        &material.signature,
        &material.chain,
        &material.anchor,
        &material.revocations,
        signed_at,
        &policy,
    )
    .map_err(signing_refusal)?;
    if verified.publisher_id != signer.publisher_id() {
        return Err(Error::Signature {
            message: "the verified publisher differs from the signer".to_owned(),
        });
    }
    Ok(SignedSource {
        source,
        signature: material.signature,
        chain: material.chain,
        publisher: verified.publisher_id,
    })
}

/// Turns a signing refusal into the crate's stable code, keeping the message plain.
fn signing_refusal(error: SigningError) -> Error {
    Error::Signature {
        message: format!("{}: {error}", error.code()),
    }
}
