//! The tenant-scoped store of provider configurations (WP-IAM-05.T9).
//!
//! A store serves **one tenant** (fixed when it is built). A configuration is the same
//! [`ProviderConfig`] the loader checks: the client secret is only the **name** of a
//! `spot-secrets` entry ([`crate::ClientAuth`]), and the audience (`staff`, `partner`,
//! `consumer`) is stored with the row.
//!
//! Every write and every read goes through [`ProviderConfig::validate`]: a configuration that
//! does not validate is refused on write ([`ProviderStoreError::Invalid`]) and, if a row has
//! become invalid since (a rule tightened, a row edited by hand), it is **not served**: `list`
//! leaves it out and names it in [`ProviderListing::skipped`], and `get` reports it as
//! [`ProviderStoreError::Invalid`]. An error from the store itself is
//! [`ProviderStoreError::Unavailable`], a fixed sentence with no detail.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use crate::error::{self as e, SsoError, SsoErrors};
use crate::provider::{ProviderConfig, ProviderId};
use crate::replay::StoreFuture;

/// The providers a tenant has, and those that were left out because they no longer validate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderListing {
    /// The valid configurations, ordered by provider id.
    pub providers: Vec<ProviderConfig>,
    /// The ids of rows that were skipped because they no longer validate (or could not be read).
    pub skipped: Vec<ProviderId>,
}

/// Why a provider store call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderStoreError {
    /// The store could not answer. Nothing was changed, nothing is known.
    Unavailable,
    /// The configuration does not validate: it was not stored, or the stored row was not served.
    Invalid(SsoErrors),
}

impl ProviderStoreError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable => e::codes::PROVIDER_STORE_UNAVAILABLE,
            Self::Invalid(_) => e::codes::PROVIDER_INVALID,
        }
    }
}

impl fmt::Display for ProviderStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("The provider store could not be reached."),
            Self::Invalid(errors) => write!(f, "The provider settings are not valid: {errors}"),
        }
    }
}

impl std::error::Error for ProviderStoreError {}

impl From<ProviderStoreError> for SsoError {
    fn from(error: ProviderStoreError) -> Self {
        match error {
            ProviderStoreError::Unavailable => SsoError::new(
                e::codes::PROVIDER_STORE_UNAVAILABLE,
                "The identity provider settings could not be read.",
                "The provider-configuration store is unavailable; restore it and try again.",
            ),
            ProviderStoreError::Invalid(_) => SsoError::new(
                e::codes::PROVIDER_INVALID,
                "The identity provider settings are not valid.",
                "Correct the provider configuration so that it passes validation, then save it again.",
            ),
        }
    }
}

/// The result of a provider store call.
pub type ProviderResult<T> = Result<T, ProviderStoreError>;

/// Keeps one tenant's provider configurations.
pub trait ProviderStore: Send + Sync {
    /// Every provider of the tenant, valid ones in `providers`, the rest in `skipped`.
    ///
    /// # Errors
    ///
    /// [`ProviderStoreError::Unavailable`] when the store cannot answer.
    fn list(&self) -> StoreFuture<'_, ProviderResult<ProviderListing>>;

    /// The provider `id`, `None` when the tenant has none (a provider of another tenant is
    /// exactly as missing).
    ///
    /// # Errors
    ///
    /// [`ProviderStoreError::Unavailable`], or [`ProviderStoreError::Invalid`] when the stored
    /// row no longer validates.
    fn get<'a>(
        &'a self,
        id: &'a ProviderId,
    ) -> StoreFuture<'a, ProviderResult<Option<ProviderConfig>>>;

    /// Creates or replaces a provider, after validating it.
    ///
    /// # Errors
    ///
    /// [`ProviderStoreError::Invalid`] (nothing stored) or [`ProviderStoreError::Unavailable`].
    fn put<'a>(&'a self, config: &'a ProviderConfig) -> StoreFuture<'a, ProviderResult<()>>;

    /// Deletes a provider; `true` when there was one.
    ///
    /// # Errors
    ///
    /// [`ProviderStoreError::Unavailable`].
    fn delete<'a>(&'a self, id: &'a ProviderId) -> StoreFuture<'a, ProviderResult<bool>>;
}

impl<T: ProviderStore + ?Sized> ProviderStore for Arc<T> {
    fn list(&self) -> StoreFuture<'_, ProviderResult<ProviderListing>> {
        (**self).list()
    }
    fn get<'a>(
        &'a self,
        id: &'a ProviderId,
    ) -> StoreFuture<'a, ProviderResult<Option<ProviderConfig>>> {
        (**self).get(id)
    }
    fn put<'a>(&'a self, config: &'a ProviderConfig) -> StoreFuture<'a, ProviderResult<()>> {
        (**self).put(config)
    }
    fn delete<'a>(&'a self, id: &'a ProviderId) -> StoreFuture<'a, ProviderResult<bool>> {
        (**self).delete(id)
    }
}

/// Validates a configuration for writing or serving; the one place both paths share.
///
/// # Errors
///
/// [`ProviderStoreError::Invalid`] with every problem.
pub fn check(config: &ProviderConfig) -> ProviderResult<()> {
    config.validate().map_err(ProviderStoreError::Invalid)
}

/// The rows of every tenant of one in-memory backend, keyed `(tenant, id)`.
type Rows = Arc<Mutex<BTreeMap<(String, ProviderId), ProviderConfig>>>;

/// An in-memory [`ProviderStore`] for tests and single-node development.
#[derive(Clone, Debug)]
pub struct MemoryProviderStore {
    tenant: String,
    rows: Rows,
}

impl Default for MemoryProviderStore {
    /// An empty store for the tenant named `default`.
    fn default() -> Self {
        Self {
            tenant: "default".to_owned(),
            rows: Rows::default(),
        }
    }
}

impl MemoryProviderStore {
    /// An empty store for the tenant named `default`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Another tenant's view of the same backend.
    #[must_use]
    pub fn for_tenant(&self, tenant: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            rows: Arc::clone(&self.rows),
        }
    }

    /// Writes a row without validating it: for the test that proves an invalid row is not
    /// served.
    pub fn put_unchecked(&self, config: ProviderConfig) {
        if let Ok(mut rows) = self.rows.lock() {
            rows.insert((self.tenant.clone(), config.id.clone()), config);
        }
    }

    fn key(&self, id: &ProviderId) -> (String, ProviderId) {
        (self.tenant.clone(), id.clone())
    }
}

impl ProviderStore for MemoryProviderStore {
    fn list(&self) -> StoreFuture<'_, ProviderResult<ProviderListing>> {
        let outcome = match self.rows.lock() {
            Ok(rows) => {
                let mut listing = ProviderListing::default();
                for ((tenant, id), config) in rows.iter() {
                    if *tenant != self.tenant {
                        continue;
                    }
                    if check(config).is_ok() {
                        listing.providers.push(config.clone());
                    } else {
                        listing.skipped.push(id.clone());
                    }
                }
                Ok(listing)
            }
            Err(_) => Err(ProviderStoreError::Unavailable),
        };
        Box::pin(std::future::ready(outcome))
    }

    fn get<'a>(
        &'a self,
        id: &'a ProviderId,
    ) -> StoreFuture<'a, ProviderResult<Option<ProviderConfig>>> {
        let outcome = match self.rows.lock() {
            Ok(rows) => match rows.get(&self.key(id)) {
                None => Ok(None),
                Some(config) => check(config).map(|()| Some(config.clone())),
            },
            Err(_) => Err(ProviderStoreError::Unavailable),
        };
        Box::pin(std::future::ready(outcome))
    }

    fn put<'a>(&'a self, config: &'a ProviderConfig) -> StoreFuture<'a, ProviderResult<()>> {
        let outcome = check(config).and_then(|()| match self.rows.lock() {
            Ok(mut rows) => {
                rows.insert(self.key(&config.id), config.clone());
                Ok(())
            }
            Err(_) => Err(ProviderStoreError::Unavailable),
        });
        Box::pin(std::future::ready(outcome))
    }

    fn delete<'a>(&'a self, id: &'a ProviderId) -> StoreFuture<'a, ProviderResult<bool>> {
        let outcome = match self.rows.lock() {
            Ok(mut rows) => Ok(rows.remove(&self.key(id)).is_some()),
            Err(_) => Err(ProviderStoreError::Unavailable),
        };
        Box::pin(std::future::ready(outcome))
    }
}
