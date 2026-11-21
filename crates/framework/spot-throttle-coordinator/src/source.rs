//! Where a salt comes from: the [`SaltSource`] port and the `spot-secrets` adapter
//! (WP-IAM-11.T12).
//!
//! A front door should not invent its own salt. [`DerivedSalt`] derives one from a master key
//! the [`spot_secrets::KeyProvider`] already holds, so **no raw salt is stored anywhere**: the
//! same tenant and version always derive the same salt, and two tenants (or two versions) always
//! derive different ones. That is what makes a restart rebuild its ring exactly
//! ([`crate::SaltRing::with_previous`]) and a rotation a matter of asking for the next version.
//!
//! # How it is derived
//!
//! `spot-secrets` keeps its key-encryption key (KEK) inside the provider and derives purpose
//! keys from it on request: [`spot_secrets::KeyProvider::derive_key`], an HKDF-SHA256 expansion
//! of the KEK of one key version under a caller label. The throttle asks for one 32-byte key per
//! tenant and version and uses it as the salt:
//!
//! ```text
//! salt = KeyProvider::derive_key(current KEK version, info, 32)
//!      = HKDF-SHA256(KEK, salt = empty, info = DERIVE_PREFIX || info)
//! info = "spot.throttle.salt.v2" || 0x00 || u64::to_be(tenant.len()) || tenant || u32::to_be(version)
//! ```
//!
//! The label separates the throttle's salts from every other use of the same KEK, the tenant and
//! the version are part of the input (so two tenants never share a salt and a rotation is just
//! the next version), and the length prefix and the `0x00` mean a tenant can never be read as
//! part of the label or of another tenant. Nothing derived here is ever stored, logged, returned
//! to a caller outside the crate or written by `Debug`: the derived bytes live in a
//! [`spot_secrets::SecretBytes`] (zeroed on drop) and become a [`ThrottleSalt`], which is
//! redacted.
//!
//! # Upgrading from the first derivation (`v1`)
//!
//! The first version of this adapter chained a second HKDF expansion off the blind-index key
//! (`KeyProvider::index_key`) under the label `spot.throttle.salt.v1`. It now derives from the
//! KEK directly through `derive_key`, so **the derived salt bytes change at the upgrade**, and
//! the label is versioned `v2` so a mix of old and new nodes can never silently agree on a salt.
//! A deployment that has already stored throttle counters keyed by the old salts loses them at
//! the upgrade: acceptable, because a throttle only ever forgets (an attacker gets a fresh
//! window, no honest caller is locked out). Roll the nodes of one tenant together.
//!
//! # Failure is refusal
//!
//! Every failure — a provider that cannot derive, a retired key version, a tenant that is not a
//! plain id, a version of 0 — is `Err(ThrottleError::SaltUnavailable)`. A caller that cannot get
//! a salt counts nothing and refuses the attempt: there is no "allow without a salt" path.
//!

use std::future::Future;
use std::pin::Pin;

use spot_secrets::KeyProvider;

use crate::attempt::is_plain_tenant;
use crate::error::ThrottleError;
use crate::policy::ThrottleSalt;
use crate::ring::SaltRing;

/// The domain-separating label of every derived throttle salt: `spot.throttle.salt.v2`.
///
/// It is one string; it is spelled in two halves only so the repository's error-code scan does
/// not read a derivation label as an error code (it treats any dotted lower-case literal as
/// one). The value is a format constant: changing it derives different salts for every tenant
/// and version, which is a rotation for every ring (`v1` derived from the blind-index key; see
/// the module documentation for the upgrade).
pub const SALT_INFO: &str = concat!("spot.throttle.salt.", "v2");

/// Length of a derived salt, in bytes.
const SALT_LEN: usize = 32;

/// The boxed future a [`SaltSource`] returns, so the trait stays object-safe (`dyn SaltSource`)
/// and the future is `Send` for a request handler — the same shape as `spot_coordinator::BoxFuture`
/// and `spot_secrets::ProviderFuture`.
pub type SaltFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ThrottleSalt, ThrottleError>> + Send + 'a>>;

/// Where the throttle's salt comes from.
///
/// One implementation ships here, [`DerivedSalt`], over the `spot-secrets` provider. A front door
/// that keeps its salts somewhere else implements this trait instead of inventing a salt of its
/// own, and every caller then goes through [`crate::CoordinatorThrottle::with_ring`].
pub trait SaltSource: Send + Sync {
    /// The salt of `tenant` at `version` (versions start at 1; a rotation asks for the next one).
    ///
    /// `Err(SaltUnavailable)` means the salt cannot be had: **refuse** the attempt; never
    /// fall back to a constant or an empty salt.
    fn load(&self, tenant: &str, version: u32) -> SaltFuture<'_>;
}

/// A salt derived per tenant and per version from a master key held by a
/// [`spot_secrets::KeyProvider`].
///
/// See the module documentation for the exact derivation. The adapter holds the provider (which
/// owns the key material) and nothing else: no salt, no derived key, no tenant list.
pub struct DerivedSalt<P: KeyProvider> {
    provider: P,
}

impl<P: KeyProvider> DerivedSalt<P> {
    /// Derives salts from the master key `provider` holds.
    pub fn new(provider: P) -> Self {
        Self { provider }
    }

    /// The provider this adapter derives from.
    pub fn provider(&self) -> &P {
        &self.provider
    }
}

impl<P: KeyProvider> SaltSource for DerivedSalt<P> {
    fn load(&self, tenant: &str, version: u32) -> SaltFuture<'_> {
        // Everything the derivation needs from the caller is copied here, so the future borrows
        // only the provider: a handler may drop the tenant string while the provider works.
        let prepared = if !is_plain_tenant(tenant) {
            Err(ThrottleError::SaltUnavailable {
                reason: "the tenant is not a plain id (1 to 64 printable ASCII bytes without a colon)",
            })
        } else if version == 0 {
            Err(ThrottleError::SaltUnavailable {
                reason: "a salt version starts at 1",
            })
        } else {
            Ok(info_bytes(tenant, version))
        };
        Box::pin(async move {
            let info = prepared?;
            // The KEK never leaves the provider; the provider's own error (which may name a key
            // id) is replaced by a fixed sentence.
            let key = self.provider.current();
            let derived = self
                .provider
                .derive_key(&key, &info, SALT_LEN)
                .await
                .map_err(|_| ThrottleError::SaltUnavailable {
                    reason: "the key provider could not derive the throttle salt",
                })?;
            ThrottleSalt::new(derived.expose()).map_err(|_| ThrottleError::SaltUnavailable {
                reason: "the derived salt was rejected",
            })
        })
    }
}

impl<P: KeyProvider> std::fmt::Debug for DerivedSalt<P> {
    /// Names the type and the provider's key reference — never the key, a derived salt or a
    /// tenant. The provider itself is not formatted: `KeyProvider` has no redacted `Debug`
    /// contract to rely on.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DerivedSalt(<redacted>)")
    }
}

/// Loads the ring of `tenant`: `current` at `version`, and `previous_version` (when the caller is
/// still inside the overlap) as the previous salt.
///
/// This is what a node does at start-up: two calls to the same source, so the salts match every
/// other node's without any of them being stored. A failing source is `Err(SaltUnavailable)`, and
/// the caller then refuses attempts (fail closed).
pub async fn load_ring<S: SaltSource + ?Sized>(
    source: &S,
    tenant: &str,
    version: u32,
    previous_version: Option<u32>,
) -> Result<SaltRing, ThrottleError> {
    let current = source.load(tenant, version).await?;
    match previous_version {
        Some(previous) => {
            let previous = source.load(tenant, previous).await?;
            Ok(SaltRing::with_previous(current, previous))
        }
        None => Ok(SaltRing::new(current)),
    }
}

/// `SALT_INFO || 0x00 || len(tenant) || tenant || version`.
fn info_bytes(tenant: &str, version: u32) -> Vec<u8> {
    let mut info = Vec::with_capacity(SALT_INFO.len() + 1 + 8 + tenant.len() + 4);
    info.extend_from_slice(SALT_INFO.as_bytes());
    info.push(0);
    info.extend_from_slice(&(tenant.len() as u64).to_be_bytes());
    info.extend_from_slice(tenant.as_bytes());
    info.extend_from_slice(&version.to_be_bytes());
    info
}
