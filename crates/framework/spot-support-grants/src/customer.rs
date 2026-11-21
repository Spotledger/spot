//! The customer side: who may approve a grant, and with which owner flag.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use spot_support_session::Approver;

/// The customer administrators per tenant, the only people who may approve or extend a
/// grant.
///
/// A tenant's list is loaded from its configuration; an approver not on it is refused
/// (`support_grants.not_admin`), which is how "creation requires customer approval"
/// is enforced rather than merely documented. The owner flag lifts the session policy's
/// `max_tier_without_owner_approval` limit, exactly as [`Approver`] defines it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CustomerAdmins {
    /// tenant id -> administrator id -> whether that administrator is a tenant owner.
    by_tenant: BTreeMap<String, BTreeMap<String, bool>>,
}

impl CustomerAdmins {
    /// An empty directory: no tenant has an approver until one is added.
    #[must_use]
    pub fn new() -> CustomerAdmins {
        CustomerAdmins::default()
    }

    /// Registers an administrator for a tenant, with the tenant-owner flag.
    pub fn add(&mut self, tenant: &str, id: &str, is_tenant_owner: bool) {
        self.by_tenant
            .entry(tenant.to_owned())
            .or_default()
            .insert(id.to_owned(), is_tenant_owner);
    }

    /// Whether `id` is a registered administrator of `tenant`.
    #[must_use]
    pub fn is_admin(&self, tenant: &str, id: &str) -> bool {
        self.by_tenant
            .get(tenant)
            .is_some_and(|admins| admins.contains_key(id))
    }

    /// The [`Approver`] record for `id` on `tenant`, or `None` when not an administrator.
    #[must_use]
    pub fn approver(&self, tenant: &str, id: &str) -> Option<Approver> {
        self.by_tenant
            .get(tenant)
            .and_then(|admins| admins.get(id))
            .map(|owner| Approver::new(id, *owner))
    }
}
