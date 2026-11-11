//! Credential references and the resolver that opens them.
//!
//! A node carries a [`CredentialRef`] only — a name — never a value. At call time the handler
//! asks the [`SecretResolver`] for the value; the resolver returns a [`SecretString`], the
//! redacting type whose `Debug` and `Display` print `[redacted]` and which deliberately does not
//! implement `Serialize`. The value is used for the duration of one call and never stored.
//!
//! The production resolver is [`SealedResolver`]: it opens a `spsec:v1:` sealed value (as stored
//! in a document or setting) through `spot-secrets`' [`SecretBox`], so the vault's envelope
//! encryption and associated-data binding apply unchanged. [`InMemorySecretResolver`] is the
//! test double.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use spot_secrets::{KeyProvider, Sealed, SecretBox, SecretString};

use crate::error::IntegrationError;

/// A credential's name. It is the only thing a node stores: the value lives in the vault and is
/// resolved at call time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialRef {
    /// The reference's name (`sales_api_token`).
    pub name: String,
}

impl CredentialRef {
    /// A reference with this name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// The reference's name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

impl From<&str> for CredentialRef {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

/// Resolves a credential reference to its value at call time.
///
/// The returned [`SecretString`] redacts itself: a caller that formats it, logs it or puts it in
/// a struct that derives `Debug` cannot leak it. The resolver never returns a value it could not
/// open, and its error message never names the value.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// Opens the credential `reference`.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::CredentialMissing`] when the vault has no such entry, or
    /// [`IntegrationError::SecretResolveFailed`] when the entry cannot be opened.
    async fn resolve(&self, reference: &CredentialRef) -> Result<SecretString, IntegrationError>;
}

/// A resolver over a fixed map of name to value, for tests and local development.
///
/// It holds plaintext values in memory on purpose: it exists to exercise the handlers, and it
/// still hands them out as [`SecretString`] so nothing downstream can print them.
#[derive(Default)]
pub struct InMemorySecretResolver {
    values: BTreeMap<String, SecretString>,
}

impl InMemorySecretResolver {
    /// An empty resolver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a value under a name. The value is never printed.
    pub fn insert(&mut self, reference: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.values
            .insert(reference.into(), SecretString::new(value.into()));
        self
    }
}

#[async_trait]
impl SecretResolver for InMemorySecretResolver {
    async fn resolve(&self, reference: &CredentialRef) -> Result<SecretString, IntegrationError> {
        self.values
            .get(reference.as_str())
            .map(|value| SecretString::new(value.expose()))
            .ok_or_else(|| IntegrationError::CredentialMissing {
                reference: reference.name.clone(),
            })
    }
}

/// One sealed value as the vault stores it: the `spsec:v1:` text and the associated data it was
/// bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedEntry {
    /// The sealed text, as `Sealed`'s `Display` writes it.
    pub sealed: String,
    /// The associated data the value was sealed under (`site=acme;doctype=Email Account`).
    pub aad: String,
}

/// The vault's storage: which sealed entry a reference names.
pub trait SealedVault: Send + Sync {
    /// The entry a reference names, or `None`.
    fn entry(&self, reference: &CredentialRef) -> Option<SealedEntry>;
}

/// A vault over a fixed map, for tests.
#[derive(Default)]
pub struct InMemoryVault {
    entries: BTreeMap<String, SealedEntry>,
}

impl InMemoryVault {
    /// An empty vault.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores an entry under a reference name.
    pub fn insert(&mut self, reference: impl Into<String>, entry: SealedEntry) -> &mut Self {
        self.entries.insert(reference.into(), entry);
        self
    }
}

impl SealedVault for InMemoryVault {
    fn entry(&self, reference: &CredentialRef) -> Option<SealedEntry> {
        self.entries.get(reference.as_str()).cloned()
    }
}

/// A resolver over `spot-secrets`: it opens the sealed entry a reference names, bound to the
/// entry's associated data.
pub struct SealedResolver<P: KeyProvider> {
    secret_box: SecretBox<P>,
    vault: Arc<dyn SealedVault>,
}

impl<P: KeyProvider> SealedResolver<P> {
    /// A resolver over `provider`, reading sealed entries from `vault`.
    pub fn new(provider: P, vault: Arc<dyn SealedVault>) -> Self {
        Self {
            secret_box: SecretBox::new(provider),
            vault,
        }
    }
}

#[async_trait]
impl<P: KeyProvider> SecretResolver for SealedResolver<P> {
    async fn resolve(&self, reference: &CredentialRef) -> Result<SecretString, IntegrationError> {
        let entry =
            self.vault
                .entry(reference)
                .ok_or_else(|| IntegrationError::CredentialMissing {
                    reference: reference.name.clone(),
                })?;
        let sealed =
            Sealed::from_str(&entry.sealed).map_err(|e| IntegrationError::SecretResolveFailed {
                reference: reference.name.clone(),
                detail: e.to_string(),
            })?;
        self.secret_box
            .open(&sealed, entry.aad.as_bytes())
            .await
            .map_err(|e| IntegrationError::SecretResolveFailed {
                reference: reference.name.clone(),
                detail: e.to_string(),
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_secrets::{MemoryKeyring, SecretBox};

    #[tokio::test]
    async fn a_reference_resolves_through_the_sealed_vault() {
        let keyring = Arc::new(MemoryKeyring::new("test").unwrap());
        let aad = "site=acme;field=token";
        let sealed = SecretBox::new(Arc::clone(&keyring))
            .seal(&SecretString::new("canary-secret-value"), aad.as_bytes())
            .await
            .unwrap();
        let mut vault = InMemoryVault::new();
        vault.insert(
            "sales_api",
            SealedEntry {
                sealed: sealed.to_string(),
                aad: aad.to_owned(),
            },
        );
        let resolver = SealedResolver::new(keyring, Arc::new(vault));
        let value = resolver
            .resolve(&CredentialRef::new("sales_api"))
            .await
            .unwrap();
        assert_eq!(value.expose(), "canary-secret-value");
        assert_eq!(format!("{value:?}"), "[redacted]");
    }

    #[tokio::test]
    async fn a_missing_reference_is_reported_without_a_value() {
        let resolver = InMemorySecretResolver::new();
        let error = resolver
            .resolve(&CredentialRef::new("nope"))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "bpm_integration.credential_missing");
        assert!(error.to_string().contains("nope"));
    }
}
