//! The server secret a grant is signed under, and the port that resolves it.
//!
//! The authority stores a [`SecretRef`] — a name — and nothing else. Every `issue` and every
//! `authenticate` asks the [`GrantSecrets`] port for the value, uses it for one HMAC and drops
//! it: [`SecretBytes`] zeroes its buffer on drop and prints `[redacted]`, so the value cannot
//! reach a `Debug` line, a log record, an error message or the grant itself.
//!
//! The port is synchronous because the [`RequestAuthority`] port it serves is: `authenticate` is
//! a plain `fn`, so a host wires a resolver that answers from the site's already-unlocked vault
//! keyring rather than a runtime-bridging async call. Resolving per call is the point — a secret
//! rotated in the vault takes effect on the next call, and nothing long-lived holds it.
//!
//! [`RequestAuthority`]: spot_bpm_governance::RequestAuthority

use std::collections::BTreeMap;
use std::fmt;

use spot_secrets::SecretBytes;

use crate::error::GrantError;

/// The name of the server secret in the vault.
///
/// A name is not a secret: it is what a host configures and what a `Debug` line may show. Only the
/// value is redacted.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretRef(String);

impl SecretRef {
    /// The reference named `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The name, for logging which secret a host wired.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SecretRef {
    fn from(name: &str) -> Self {
        Self(name.to_owned())
    }
}

/// Resolves the server secret a grant is signed under, on every call.
///
/// An implementation either answers with the value the vault holds for [`SecretRef`] or with
/// [`GrantError::SecretUnavailable`]; it must not cache the value in the authority, and it must
/// not put vault detail into the error (a resolver's failure text can name a key path or a
/// driver, which is why this crate never asks for it).
pub trait GrantSecrets: Send + Sync {
    /// The value of `reference`, zeroed when the caller drops it.
    ///
    /// # Errors
    /// [`GrantError::SecretUnavailable`] when the vault has no such secret or cannot open it.
    fn resolve(&self, reference: &SecretRef) -> Result<SecretBytes, GrantError>;
}

/// A [`GrantSecrets`] over a fixed map, for tests and single-site hosts.
///
/// Values are held as [`SecretBytes`], so a `Debug` line of this resolver prints `[redacted]` even
/// though the values are in memory.
#[derive(Debug, Default)]
pub struct FixedSecrets {
    values: BTreeMap<String, SecretBytes>,
}

impl FixedSecrets {
    /// An empty resolver: every resolve fails, so every issue fails closed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `value` under `reference`.
    pub fn insert(&mut self, reference: impl Into<String>, value: impl AsRef<[u8]>) -> &mut Self {
        self.values
            .insert(reference.into(), SecretBytes::new(value.as_ref().to_vec()));
        self
    }

    /// Wraps the resolver for sharing.
    #[must_use]
    pub fn shared(self) -> std::sync::Arc<Self> {
        std::sync::Arc::new(self)
    }
}

impl GrantSecrets for FixedSecrets {
    fn resolve(&self, reference: &SecretRef) -> Result<SecretBytes, GrantError> {
        self.values
            .get(reference.as_str())
            .map(|value| SecretBytes::new(value.expose().to_vec()))
            .ok_or(GrantError::SecretUnavailable)
    }
}
