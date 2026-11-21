//! Data residency: the target region must be one the tenant's policy allows.
//!
//! ADR-058 rule 1: the tenant is the unit of residency, and a move across regions is
//! refused unless the owner approves a change of region. The engine asks the policy
//! **before** it quiesces the source, so a refused move never interrupts service.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_tenancy::{ControlStore, Region, TenantId};

use crate::error::{Error, Result};
use crate::plan::MovePlan;

/// The policy a move asks about its target region.
pub trait ResidencyPolicy: Send + Sync {
    /// Allows or refuses `plan`'s target region.
    ///
    /// # Errors
    /// [`Error::ResidencyRefused`] when the region is not allowed; [`Error::StoreFailed`]
    /// when the policy could not be read.
    fn check(&self, plan: &MovePlan) -> Result<()>;
}

/// A fixed allow-list per tenant, for hosts that keep residency data outside the control
/// store (and for tests).
#[derive(Debug, Clone, Default)]
pub struct AllowedRegions {
    allowed: BTreeMap<TenantId, BTreeSet<Region>>,
}

impl AllowedRegions {
    /// An empty policy that allows nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allows `region` for `tenant`.
    pub fn allow(&mut self, tenant: TenantId, region: Region) {
        self.allowed.entry(tenant).or_default().insert(region);
    }

    /// The same policy with `region` allowed (builder form).
    #[must_use]
    pub fn with(mut self, tenant: TenantId, region: Region) -> Self {
        self.allow(tenant, region);
        self
    }
}

impl ResidencyPolicy for AllowedRegions {
    fn check(&self, plan: &MovePlan) -> Result<()> {
        if self
            .allowed
            .get(&plan.tenant)
            .is_some_and(|regions| regions.contains(&plan.region))
        {
            Ok(())
        } else {
            Err(Error::ResidencyRefused {
                tenant: plan.tenant.to_string(),
                region: plan.region.to_string(),
            })
        }
    }
}

/// The control-store policy: a move stays inside the tenant's recorded region unless an
/// owner approval for the destination region was recorded here.
pub struct TenancyResidency<S: ControlStore + Send + Sync> {
    store: Arc<S>,
    approvals: BTreeMap<TenantId, BTreeSet<Region>>,
}

impl<S: ControlStore + Send + Sync> TenancyResidency<S> {
    /// A policy over the control store with no approved region changes.
    #[must_use]
    pub fn new(store: Arc<S>) -> Self {
        Self {
            store,
            approvals: BTreeMap::new(),
        }
    }

    /// Records the owner's approval for `tenant` to move to `region`.
    #[must_use]
    pub fn approve(mut self, tenant: TenantId, region: Region) -> Self {
        self.approvals.entry(tenant).or_default().insert(region);
        self
    }
}

impl<S: ControlStore + Send + Sync> ResidencyPolicy for TenancyResidency<S> {
    fn check(&self, plan: &MovePlan) -> Result<()> {
        let tenant = self
            .store
            .get(&plan.tenant)
            .map_err(|_| Error::StoreFailed {
                tenant: plan.tenant.to_string(),
                reason: "the tenant could not be read for its residency policy",
            })?;
        let approved = self
            .approvals
            .get(&plan.tenant)
            .is_some_and(|regions| regions.contains(&plan.region));
        if plan.region == tenant.region || approved {
            Ok(())
        } else {
            Err(Error::ResidencyRefused {
                tenant: plan.tenant.to_string(),
                region: plan.region.to_string(),
            })
        }
    }
}
