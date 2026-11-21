//! The durable credential vault: values sealed at rest, keyed by `(site, reference)`.
//!
//! A value is written once through [`DurableVault::put`] (a write-only action: there is no read
//! that returns it) and stored only as a `spot-secrets` [`Sealed`] blob, bound to its site and
//! reference by the associated data, so a copy moved to another reference or site fails to open. It
//! is opened only by [`CredentialVault::resolve_for`], which the host calls from the `secrets-get`
//! import (for a reference the connector declared) and when it injects an `auth` credential.
//!
//! **Tenancy.** The vault belongs to one site and refuses a call for another tenant
//! (`SecretUnavailable`), so two sites with a connector of the same name never share a credential.
//!
//! **Rotation.** [`DurableVault::reseal_all`] opens every value under the key it was sealed with
//! and seals it again under the provider's current key; a replaced secret takes effect on the next
//! run because nothing is cached here.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use spot_connectors::error::{ConnectorError, Result as ConnResult};
use spot_connectors::{CredentialRef, CredentialVault};
use spot_secrets::{KeyProvider, Sealed, SecretBox, SecretString};

use super::defs::{Boxed, STORE_FAILED};

/// `connector.secret_unreadable`: a stored value could not be opened (wrong key, tampered).
pub const SECRET_UNREADABLE: &str = "connector.secret_unreadable";

/// Where sealed blobs live.
pub trait SealedStore: Send + Sync {
    /// Stores `blob` under `(site, reference)`, replacing any.
    fn put<'a>(
        &'a self,
        site: &'a str,
        reference: &'a str,
        blob: Vec<u8>,
    ) -> Boxed<'a, Result<(), String>>;
    /// The blob under `(site, reference)`.
    fn get<'a>(
        &'a self,
        site: &'a str,
        reference: &'a str,
    ) -> Boxed<'a, Result<Option<Vec<u8>>, String>>;
    /// Every `(reference, blob)` of `site`.
    #[allow(clippy::type_complexity)]
    fn all<'a>(&'a self, site: &'a str) -> Boxed<'a, Result<Vec<(String, Vec<u8>)>, String>>;
}

/// An in-memory store (tests).
#[derive(Default)]
pub struct MemorySealedStore {
    items: Mutex<BTreeMap<(String, String), Vec<u8>>>,
}

impl MemorySealedStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The raw bytes held for a reference, to prove they are not the value.
    #[must_use]
    pub fn raw(&self, site: &str, reference: &str) -> Option<Vec<u8>> {
        self.items
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&(site.to_owned(), reference.to_owned()))
            .cloned()
    }
}

impl SealedStore for MemorySealedStore {
    fn put<'a>(
        &'a self,
        site: &'a str,
        reference: &'a str,
        blob: Vec<u8>,
    ) -> Boxed<'a, Result<(), String>> {
        Box::pin(async move {
            self.items
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert((site.to_owned(), reference.to_owned()), blob);
            Ok(())
        })
    }

    fn get<'a>(
        &'a self,
        site: &'a str,
        reference: &'a str,
    ) -> Boxed<'a, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move { Ok(self.raw(site, reference)) })
    }

    fn all<'a>(&'a self, site: &'a str) -> Boxed<'a, Result<Vec<(String, Vec<u8>)>, String>> {
        Box::pin(async move {
            Ok(self
                .items
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .filter(|((s, _), _)| s == site)
                .map(|((_, r), b)| (r.clone(), b.clone()))
                .collect())
        })
    }
}

/// The PostgreSQL store, table `spot_connector_secret`.
pub struct PgSealedStore {
    pool: deadpool_postgres::Pool,
}

impl PgSealedStore {
    /// A store over `pool`.
    #[must_use]
    pub fn new(pool: deadpool_postgres::Pool) -> Self {
        Self { pool }
    }

    /// Creates the table if it is missing.
    ///
    /// # Errors
    ///
    /// `connector.store_failed`.
    pub async fn prepare(&self) -> Result<(), String> {
        let client = self.pool.get().await.map_err(|_| STORE_FAILED.to_owned())?;
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS spot_connector_secret (\
                 site text NOT NULL, reference text NOT NULL, sealed bytea NOT NULL, \
                 PRIMARY KEY (site, reference))",
            )
            .await
            .map_err(|_| STORE_FAILED.to_owned())
    }
}

impl SealedStore for PgSealedStore {
    fn put<'a>(
        &'a self,
        site: &'a str,
        reference: &'a str,
        blob: Vec<u8>,
    ) -> Boxed<'a, Result<(), String>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| STORE_FAILED.to_owned())?;
            client
                .execute(
                    "INSERT INTO spot_connector_secret (site, reference, sealed) \
                     VALUES ($1, $2, $3) \
                     ON CONFLICT (site, reference) DO UPDATE SET sealed = EXCLUDED.sealed",
                    &[&site, &reference, &blob],
                )
                .await
                .map(|_| ())
                .map_err(|_| STORE_FAILED.to_owned())
        })
    }

    fn get<'a>(
        &'a self,
        site: &'a str,
        reference: &'a str,
    ) -> Boxed<'a, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| STORE_FAILED.to_owned())?;
            let rows = client
                .query(
                    "SELECT sealed FROM spot_connector_secret WHERE site = $1 AND reference = $2",
                    &[&site, &reference],
                )
                .await
                .map_err(|_| STORE_FAILED.to_owned())?;
            Ok(rows.first().map(|row| row.get::<_, Vec<u8>>(0)))
        })
    }

    fn all<'a>(&'a self, site: &'a str) -> Boxed<'a, Result<Vec<(String, Vec<u8>)>, String>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| STORE_FAILED.to_owned())?;
            let rows = client
                .query(
                    "SELECT reference, sealed FROM spot_connector_secret WHERE site = $1",
                    &[&site],
                )
                .await
                .map_err(|_| STORE_FAILED.to_owned())?;
            Ok(rows
                .iter()
                .map(|row| (row.get::<_, String>(0), row.get::<_, Vec<u8>>(1)))
                .collect())
        })
    }
}

/// The vault of one site.
pub struct DurableVault<P: KeyProvider> {
    site: String,
    sealer: SecretBox<P>,
    store: Arc<dyn SealedStore>,
}

fn aad(site: &str, reference: &str) -> Vec<u8> {
    format!("spot-connector-secret|{site}|{reference}").into_bytes()
}

impl<P: KeyProvider + 'static> DurableVault<P> {
    /// A vault for `site` sealing with `provider` (the site's own keyring).
    pub fn new(site: &str, provider: P, store: Arc<dyn SealedStore>) -> Self {
        Self {
            site: site.to_owned(),
            sealer: SecretBox::new(provider),
            store,
        }
    }

    /// Seals `value` and stores it under `reference`. Write-only: nothing returns it.
    ///
    /// # Errors
    ///
    /// `connector.secret_unreadable` when sealing fails, `connector.store_failed` when storing does.
    pub async fn put(&self, reference: &str, value: &SecretString) -> Result<(), String> {
        let sealed = self
            .sealer
            .seal(value, &aad(&self.site, reference))
            .await
            .map_err(|_| SECRET_UNREADABLE.to_owned())?;
        self.store
            .put(&self.site, reference, sealed.to_bytes())
            .await
    }

    /// Seals every stored value again under the provider's current key; returns how many.
    ///
    /// # Errors
    ///
    /// The first failure; values already re-sealed stay re-sealed and the rest are untouched.
    pub async fn reseal_all(&self) -> Result<usize, String> {
        let mut n = 0;
        for (reference, blob) in self.store.all(&self.site).await? {
            let sealed = Sealed::from_bytes(&blob).map_err(|_| SECRET_UNREADABLE.to_owned())?;
            let value = self
                .sealer
                .open(&sealed, &aad(&self.site, &reference))
                .await
                .map_err(|_| SECRET_UNREADABLE.to_owned())?;
            self.put(&reference, &value).await?;
            n += 1;
        }
        Ok(n)
    }

    /// Whether a value is stored under `reference` (never the value).
    ///
    /// # Errors
    ///
    /// `connector.store_failed`.
    pub async fn contains(&self, reference: &str) -> Result<bool, String> {
        Ok(self.store.get(&self.site, reference).await?.is_some())
    }

    async fn open(&self, reference: &CredentialRef) -> ConnResult<SecretString> {
        let unavailable = || ConnectorError::SecretUnavailable {
            reference: reference.to_string(),
        };
        let blob = self
            .store
            .get(&self.site, reference.as_str())
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        let sealed = Sealed::from_bytes(&blob).map_err(|_| unavailable())?;
        self.sealer
            .open(&sealed, &aad(&self.site, reference.as_str()))
            .await
            .map_err(|_| unavailable())
    }
}

#[async_trait]
impl<P: KeyProvider + 'static> CredentialVault for DurableVault<P> {
    async fn resolve(
        &self,
        reference: &CredentialRef,
        _now_micros: i64,
    ) -> ConnResult<SecretString> {
        self.open(reference).await
    }

    async fn resolve_for(
        &self,
        tenant: &str,
        reference: &CredentialRef,
        _now_micros: i64,
    ) -> ConnResult<SecretString> {
        // One site, one vault: another tenant's call finds nothing.
        if tenant != self.site {
            return Err(ConnectorError::SecretUnavailable {
                reference: reference.to_string(),
            });
        }
        self.open(reference).await
    }
}
