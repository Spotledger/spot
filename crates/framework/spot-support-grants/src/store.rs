//! The grant store: the control plane's record of who may act, until when.

use std::collections::BTreeMap;

use crate::error::GrantError;
use crate::model::Grant;

/// Where grants live.
///
/// The trait is deliberately small and has no delete: a revoked grant stays in the store
/// as evidence, and a new grant is the only way back. A backend (the PostgreSQL control
/// store) implements the same four operations.
pub trait GrantStore {
    /// The grant `id` of `tenant`, if any.
    ///
    /// # Errors
    ///
    /// The backend's failure.
    fn get(&self, tenant: &str, id: &str) -> Result<Option<Grant>, GrantError>;

    /// Stores a new grant; refuses an existing id.
    ///
    /// # Errors
    ///
    /// [`GrantError::DuplicateGrant`] or the backend's failure.
    fn insert(&mut self, grant: Grant) -> Result<(), GrantError>;

    /// Replaces an existing grant (revocation, extension); refuses an unknown id.
    ///
    /// # Errors
    ///
    /// [`GrantError::UnknownGrant`] or the backend's failure.
    fn replace(&mut self, grant: Grant) -> Result<(), GrantError>;

    /// Every grant of `tenant`, ordered by id.
    ///
    /// # Errors
    ///
    /// The backend's failure.
    fn list(&self, tenant: &str) -> Result<Vec<Grant>, GrantError>;
}

/// The reference in-memory store.
#[derive(Clone, Debug, Default)]
pub struct MemoryGrantStore {
    by_tenant: BTreeMap<String, BTreeMap<String, Grant>>,
}

impl MemoryGrantStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> MemoryGrantStore {
        MemoryGrantStore::default()
    }
}

impl GrantStore for MemoryGrantStore {
    fn get(&self, tenant: &str, id: &str) -> Result<Option<Grant>, GrantError> {
        Ok(self
            .by_tenant
            .get(tenant)
            .and_then(|grants| grants.get(id))
            .cloned())
    }

    fn insert(&mut self, grant: Grant) -> Result<(), GrantError> {
        grant.validate()?;
        let grants = self.by_tenant.entry(grant.tenant().to_owned()).or_default();
        if grants.contains_key(&grant.id) {
            return Err(GrantError::DuplicateGrant {
                tenant: grant.tenant().to_owned(),
                id: grant.id.clone(),
            });
        }
        grants.insert(grant.id.clone(), grant);
        Ok(())
    }

    fn replace(&mut self, grant: Grant) -> Result<(), GrantError> {
        grant.validate()?;
        let grants = self.by_tenant.entry(grant.tenant().to_owned()).or_default();
        if !grants.contains_key(&grant.id) {
            return Err(GrantError::UnknownGrant {
                tenant: grant.tenant().to_owned(),
                id: grant.id.clone(),
            });
        }
        grants.insert(grant.id.clone(), grant);
        Ok(())
    }

    fn list(&self, tenant: &str) -> Result<Vec<Grant>, GrantError> {
        Ok(self
            .by_tenant
            .get(tenant)
            .map(|grants| grants.values().cloned().collect())
            .unwrap_or_default())
    }
}
