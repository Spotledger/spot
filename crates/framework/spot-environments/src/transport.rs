//! Transport between environments: a bundle, a signed envelope, and the one verification the
//! receiving environment runs before it plans anything.
//!
//! The cryptography is `spot-bundle-signing`'s Ed25519 signature over the bundle digest. The
//! signature's `app_id` field carries the transport identity — set, source, target, version and
//! creator — so a signature cannot be replayed to another environment. The digest is the
//! customization bundle's canonical SHA-256.

use spot_bundle_signing::{
    BundleFacts, BundleSignature, CertificateChain, KeyPair, RevocationList, SignedCertificate,
    SigningError, TrustAnchor, VerifyMode, VerifyPolicy, sign_bundle, verify,
};
use spot_custom_bundle::{Bundle, Limits, SectionRegistry};
use spot_tenancy::TenantId;

use crate::error::{EnvironmentError, Result, codes};
use crate::model::{Environment, EnvironmentSet, Role};

/// One environment as a transport address: the set it belongs to, its role and its tenant.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnvRef {
    /// The environment set's name.
    pub set: String,
    /// The member's role.
    pub role: Role,
    /// The member's tenant.
    pub tenant: TenantId,
}

impl EnvRef {
    /// The address of one member of `set`.
    #[must_use]
    pub fn new(set: &EnvironmentSet, environment: &Environment) -> Self {
        Self {
            set: set.name().to_owned(),
            role: environment.role,
            tenant: environment.tenant,
        }
    }
}

/// The identity a transport signature covers in its `app_id` field.
fn transport_id(
    source: &EnvRef,
    target: &EnvRef,
    bundle_version: &str,
    created_by: &str,
) -> String {
    format!(
        "spotledger-env-transport-v1|{}|{}|{}|{}|{}|{}|{}",
        source.set,
        source.role.name(),
        source.tenant,
        target.role.name(),
        target.tenant,
        bundle_version,
        created_by,
    )
}

/// The signed envelope the receiver verifies before it touches the bundle.
#[derive(Clone, Debug)]
pub struct Envelope {
    /// The environment that sent the bundle.
    pub source: EnvRef,
    /// The environment the bundle is addressed to.
    pub target: EnvRef,
    /// The bundle's canonical digest, lower-case hex.
    pub digest: String,
    /// The bundle's own version.
    pub bundle_version: String,
    /// Who created the transport package.
    pub created_by: String,
    /// When it was signed (UTC seconds), supplied by the caller.
    pub signed_at: i64,
    /// The detached signature over the digest and the transport identity.
    pub signature: BundleSignature,
    /// The publisher certificate that chains to the store's trust anchor.
    pub certificate: CertificateChain,
}

impl Envelope {
    /// The identity the signature covers in its `app_id` field.
    #[must_use]
    pub fn transport_id(&self) -> String {
        transport_id(
            &self.source,
            &self.target,
            &self.bundle_version,
            &self.created_by,
        )
    }
}

/// A bundle in transit: the envelope and the exact archive bytes it signs.
#[derive(Clone, Debug)]
pub struct TransportPackage {
    /// The signed envelope.
    pub envelope: Envelope,
    /// The bundle's archive bytes.
    pub bundle: Vec<u8>,
}

/// The signing identity the sender uses.
pub struct Signer<'a> {
    /// The publisher certificate the signature names.
    pub certificate: &'a SignedCertificate,
    /// The key the certificate names.
    pub key: &'a KeyPair,
}

/// What the receiver trusts: the roots, the revocation list and the time.
pub struct Trust<'a> {
    /// The store's trusted roots.
    pub anchor: &'a TrustAnchor,
    /// The current revocation list.
    pub revocations: &'a RevocationList,
    /// The instant the check runs at (UTC seconds), supplied by the caller.
    pub now: i64,
}

/// A package whose signature, digest and route all hold.
#[derive(Debug)]
pub struct VerifiedPackage {
    bundle: Bundle,
    source: Environment,
    target: Environment,
    publisher: String,
    signed_at: i64,
}

impl VerifiedPackage {
    /// The opened bundle.
    #[must_use]
    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }

    /// The source environment.
    #[must_use]
    pub fn source(&self) -> &Environment {
        &self.source
    }

    /// The target environment.
    #[must_use]
    pub fn target(&self) -> &Environment {
        &self.target
    }

    /// The publisher the certificate names.
    #[must_use]
    pub fn publisher(&self) -> &str {
        &self.publisher
    }

    /// When the package was signed (UTC seconds).
    #[must_use]
    pub fn signed_at(&self) -> i64 {
        self.signed_at
    }
}

/// Packages a bundle for transport from one member of `set` to another.
///
/// # Errors
/// [`codes::SKIP_REFUSED`] (or another route error) when the promotion is not allowed,
/// [`codes::INVALID_BUNDLE`] for a malformed digest, and [`codes::KEY_MISMATCH`] when the key is
/// not the certificate's.
pub fn transport(
    bundle: &Bundle,
    set: &EnvironmentSet,
    from: &Environment,
    to: &Environment,
    created_by: &str,
    signer: &Signer<'_>,
    signed_at: i64,
) -> Result<TransportPackage> {
    set.route(from, to)?;
    if created_by.is_empty() {
        return Err(EnvironmentError::new(
            codes::INVALID_SET,
            "a transport package names who created it",
        ));
    }
    let source = EnvRef::new(set, from);
    let target = EnvRef::new(set, to);
    let digest_hex = bundle.digest().to_hex();
    let bundle_version = bundle.header().bundle_version.clone();
    let identity = transport_id(&source, &target, &bundle_version, created_by);
    let digest = parse_digest(&digest_hex)?;
    let signature = sign_bundle(
        &digest,
        &identity,
        &bundle_version,
        signer.certificate,
        signer.key,
        signed_at,
    )
    .map_err(signing_error)?;
    Ok(TransportPackage {
        envelope: Envelope {
            source,
            target,
            digest: digest_hex,
            bundle_version,
            created_by: created_by.to_owned(),
            signed_at,
            signature,
            certificate: CertificateChain::new(signer.certificate.clone()),
        },
        bundle: bundle.to_bytes(),
    })
}

impl TransportPackage {
    /// Verifies the package for `set` and `target`: the digest and the signature first, then
    /// the source/target membership and the promotion route.
    ///
    /// # Errors
    /// [`codes::UNKNOWN_SIGNER`] for an issuer the anchor does not trust,
    /// [`codes::SIGNATURE_INVALID`] for a bad signature, [`codes::DIGEST_MISMATCH`] or
    /// [`codes::VERSION_MISMATCH`] when the envelope does not describe the bundle,
    /// [`codes::WRONG_TARGET`] when it is addressed elsewhere, and the route errors.
    pub fn verify(
        &self,
        set: &EnvironmentSet,
        target: &Environment,
        trust: &Trust<'_>,
        registry: &SectionRegistry,
        limits: &Limits,
    ) -> Result<VerifiedPackage> {
        let bundle = Bundle::open(&self.bundle, limits, registry, false).map_err(|errors| {
            let detail = errors
                .first()
                .map_or("custom_bundle.bad_archive", |err| err.code());
            EnvironmentError::about(
                codes::INVALID_BUNDLE,
                detail,
                "the package does not carry a readable customization bundle",
            )
        })?;

        if self.envelope.source.set != set.name() || self.envelope.target.set != set.name() {
            return Err(EnvironmentError::about(
                codes::WRONG_TARGET,
                &self.envelope.target.set,
                "the package belongs to another environment set",
            ));
        }
        if self.envelope.target.tenant != target.tenant || self.envelope.target.role != target.role
        {
            return Err(EnvironmentError::about(
                codes::WRONG_TARGET,
                self.envelope.target.tenant.to_string(),
                "the package is addressed to another environment",
            ));
        }
        let source = set
            .member(&self.envelope.source.tenant)
            .filter(|member| member.role == self.envelope.source.role)
            .cloned()
            .ok_or_else(|| {
                EnvironmentError::about(
                    codes::UNKNOWN_MEMBER,
                    self.envelope.source.tenant.to_string(),
                    "the source environment is not a member of this set",
                )
            })?;
        let target_member = set
            .member(&target.tenant)
            .filter(|member| member.role == target.role)
            .cloned()
            .ok_or_else(|| {
                EnvironmentError::about(
                    codes::UNKNOWN_MEMBER,
                    target.tenant.to_string(),
                    "the target environment is not a member of this set",
                )
            })?;

        // 1. The digest over the exact bytes.
        let actual = bundle.digest().to_hex();
        if actual != self.envelope.digest {
            return Err(EnvironmentError::about(
                codes::DIGEST_MISMATCH,
                &self.envelope.digest,
                "the envelope's digest is not the bundle's digest",
            ));
        }
        if bundle.header().bundle_version != self.envelope.bundle_version {
            return Err(EnvironmentError::about(
                codes::VERSION_MISMATCH,
                &self.envelope.bundle_version,
                "the envelope's version is not the bundle's version",
            ));
        }

        // 2. The signature over that digest and the transport identity.
        let digest = parse_digest(&actual)?;
        let facts = BundleFacts {
            digest: &digest,
            app_id: &self.envelope.transport_id(),
            version: &self.envelope.bundle_version,
        };
        let policy = VerifyPolicy {
            mode: VerifyMode::Install,
            allow_expired_certificate_at_startup: false,
            held_revocation_generation: 0,
        };
        let verified = verify(
            &facts,
            &self.envelope.signature,
            &self.envelope.certificate,
            trust.anchor,
            trust.revocations,
            trust.now,
            &policy,
        )
        .map_err(signing_error)?;

        // 3. The promotion route, after the package is known to be genuine.
        set.route(&source, &target_member)?;

        Ok(VerifiedPackage {
            bundle,
            source,
            target: target_member,
            publisher: verified.publisher_id,
            signed_at: verified.signed_at,
        })
    }
}

/// The digest type `spot-bundle-signing` names, parsed from hex.
fn parse_digest<T: std::str::FromStr>(hex: &str) -> Result<T> {
    hex.parse().map_err(|_| {
        EnvironmentError::about(
            codes::INVALID_BUNDLE,
            hex,
            "the bundle digest is not 64 hexadecimal characters",
        )
    })
}

/// Maps a signing refusal onto the environment catalogue.
fn signing_error(err: SigningError) -> EnvironmentError {
    match err {
        SigningError::UnknownRoot => EnvironmentError::new(
            codes::UNKNOWN_SIGNER,
            "the package is signed by a root this store does not trust",
        ),
        SigningError::KeyMismatch => EnvironmentError::new(
            codes::KEY_MISMATCH,
            "the signing key is not the key the certificate names",
        ),
        SigningError::BundleRevoked => {
            EnvironmentError::new(codes::BUNDLE_REVOKED, "this exact bundle digest is revoked")
        }
        other => EnvironmentError::new(codes::SIGNATURE_INVALID, other.to_string()),
    }
}
