use crate::{RegistryError, Result};
use semver::{Version, VersionReq};
use spot_bundle::Digest;
use spot_bundle_signing::{BundleSignature, CertificateChain, KeyId};
use spot_manifest::{AppDependency, Capabilities};
use std::str::FromStr;

/// Manifest-compatible lowercase storage name, starting with a letter, at most 63 bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppName(String);
impl AppName {
    /// Validates a manifest name.
    pub fn parse(s: &str) -> Result<Self> {
        if s.len() > 63
            || !s.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            || !s
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(RegistryError::new(
                "registry.invalid_name",
                "invalid app name",
            ));
        }
        Ok(Self(s.to_owned()))
    }
    /// The manifest text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl FromStr for AppName {
    type Err = RegistryError;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}
/// Publisher administrative status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublisherStatus {
    /// May publish and accept transfers.
    Active,
    /// Administrative suspension; does not silently remove existing installs.
    Suspended,
}
/// Identity from an authenticated certificate, never bundle claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publisher {
    /// Stable certificate publisher ID.
    pub id: String,
    /// Authenticated display name.
    pub display_name: String,
    /// Issuing trust root.
    pub certificate_chain_root: KeyId,
    /// Administrative state.
    pub status: PublisherStatus,
}
/// Exclusive ownership and replay-resistant transfer sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ownership {
    /// Owned app.
    pub app: AppName,
    /// Current publisher.
    pub publisher: String,
    /// Starts at zero and increases on every transfer, including transfers back.
    pub generation: u64,
}
/// Reasoned unavailability marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    /// UTC seconds.
    pub at: i64,
    /// Human explanation.
    pub reason: String,
}
/// Detached authenticated release evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureRef {
    /// Bundle signature.
    pub signature: BundleSignature,
    /// Publisher certificate chain.
    pub chain: CertificateChain,
}
/// Immutable release identity plus mutable removal state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// Manifest app.
    pub app: AppName,
    /// Manifest semantic version (build metadata is not a distinct publishable version).
    pub version: Version,
    /// Canonical bundle digest.
    pub digest: Digest,
    /// Stored archive bytes.
    pub size: u64,
    /// Manifest host constraint.
    pub requires_host: Option<VersionReq>,
    /// Manifest dependency constraints.
    pub depends: Vec<AppDependency>,
    /// UTC seconds.
    pub published_at: i64,
    /// Certificate publisher.
    pub publisher: String,
    /// Unavailable for new installs; digest fetch remains possible.
    pub yanked: Option<Removal>,
    /// Security removal; all fetches refused.
    pub withdrawn: Option<Removal>,
    /// Evidence used for every fetch verification.
    pub signature_ref: SignatureRef,
    /// Signed manifest title.
    pub title: String,
    /// Catalogue description, curated by an authenticated owner (manifest lacks this field).
    pub description: String,
    /// Catalogue category, curated by an authenticated owner.
    pub category: String,
    /// Signed manifest capabilities.
    pub capabilities: Capabilities,
}
impl Release {
    /// Whether this release is available for new installs.
    pub fn active(&self) -> bool {
        self.yanked.is_none() && self.withdrawn.is_none()
    }
}
/// Visibility for listings. Inactive access is verified against publisher ownership.
#[derive(Clone, Debug, Default)]
pub struct Visibility {
    /// Include pre-release versions.
    pub include_pre: bool,
    /// Request inactive releases; requires an authenticated caller.
    pub include_inactive: bool,
}
/// Reserved private authorization signing identity, deliberately not a valid manifest
/// AppName: a distributed bundle can never carry this authority.
pub const AUTHORIZATION_APP_ID: &str = "spotledger-registry-authorization-v1";
/// Authorization protocol version, independent of app releases.
pub const AUTHORIZATION_VERSION: &str = "1.0.0";
/// Maximum lifetime of private bearer authorization evidence, in whole seconds.
pub const AUTHORIZATION_LIFETIME_SECONDS: i64 = 300;

/// A short-lived identity produced by private authorization signature verification,
/// not a caller-supplied ID or a publicly distributed release signature.
#[derive(Clone, Debug)]
pub struct Caller {
    pub(crate) publisher: String,
    pub(crate) proof: SignatureRef,
    pub(crate) facts: (Digest, String, String),
}
impl Caller {
    /// The certificate publisher ID.
    pub fn publisher(&self) -> &str {
        &self.publisher
    }
}
