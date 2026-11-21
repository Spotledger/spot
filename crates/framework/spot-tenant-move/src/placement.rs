//! Cut-over: flip the placement map and invalidate the routing caches.
//!
//! The flip is one control-store write: the tenant's placement becomes the target and its
//! state becomes `Active` in the same update. The one write before it (moving the tenant
//! to `Moving`) is the control store's precondition for a placement change. A move that
//! fails before the flip is rolled back by [`PlacementPort::restore`].

use std::sync::Arc;

use spot_tenancy::{ControlStore, TenantId, TenantState};

use crate::BoxFuture;
use crate::error::{Error, Result};
use crate::plan::MovePlan;

/// The routing caches that must forget a tenant after its placement changes.
pub trait RoutingCacheInvalidator: Send + Sync {
    /// Invalidates every cached route for `tenant`.
    ///
    /// # Errors
    /// A fixed reason (never a cache key, which may name a customer).
    fn invalidate(&self, tenant: &TenantId) -> std::result::Result<(), &'static str>;
}

/// The placement map the move cuts over.
pub trait PlacementPort: Send + Sync {
    /// Points the tenant at `plan.to` and invalidates its routes. Idempotent.
    ///
    /// # Errors
    /// [`Error::CutOverFailed`] with a fixed reason.
    fn cut_over<'a>(&'a self, plan: &'a MovePlan, now: u64) -> BoxFuture<'a, Result<()>>;

    /// Restores `plan.from` and `Active` after a failed or aborted move. Idempotent and a
    /// no-op when the placement was never flipped.
    ///
    /// # Errors
    /// [`Error::CutOverFailed`] with a fixed reason.
    fn restore<'a>(&'a self, plan: &'a MovePlan) -> BoxFuture<'a, Result<()>>;
}

/// The production placement adapter over the `spot-tenancy` control store.
pub struct ControlStorePlacement<S: ControlStore + Send + Sync> {
    store: Arc<S>,
    caches: Arc<dyn RoutingCacheInvalidator>,
}

impl<S: ControlStore + Send + Sync> ControlStorePlacement<S> {
    /// An adapter over the control store and the routing caches.
    #[must_use]
    pub fn new(store: Arc<S>, caches: Arc<dyn RoutingCacheInvalidator>) -> Self {
        Self { store, caches }
    }

    fn fail(&self, tenant: &TenantId) -> Error {
        Error::CutOverFailed {
            tenant: tenant.to_string(),
            reason: "the control store refused the placement change",
        }
    }

    fn invalidate(&self, tenant: &TenantId) -> Result<()> {
        self.caches
            .invalidate(tenant)
            .map_err(|reason| Error::CutOverFailed {
                tenant: tenant.to_string(),
                reason,
            })
    }
}

impl<S: ControlStore + Send + Sync> PlacementPort for ControlStorePlacement<S> {
    fn cut_over<'a>(&'a self, plan: &'a MovePlan, _now: u64) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let tenant = self
                .store
                .get(&plan.tenant)
                .map_err(|_| self.fail(&plan.tenant))?;
            if tenant.placement == plan.to {
                // A retried cut-over: the flip landed, only the caches may be stale.
                return self.invalidate(&plan.tenant);
            }
            if tenant.placement != plan.from {
                // The plan is stale: the tenant is not where this move started.
                return Err(self.fail(&plan.tenant));
            }
            let version = tenant.version;
            let mut current = tenant;
            if current.state != TenantState::Moving {
                if !matches!(current.state, TenantState::Active | TenantState::Suspended) {
                    return Err(self.fail(&plan.tenant));
                }
                current.state = TenantState::Moving;
                current = self
                    .store
                    .update(current, version)
                    .map_err(|_| self.fail(&plan.tenant))?;
            }
            current.placement = plan.to.clone();
            current.state = TenantState::Active;
            let version = current.version;
            self.store
                .update(current, version)
                .map_err(|_| self.fail(&plan.tenant))?;
            self.invalidate(&plan.tenant)
        })
    }

    fn restore<'a>(&'a self, plan: &'a MovePlan) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut tenant = self
                .store
                .get(&plan.tenant)
                .map_err(|_| self.fail(&plan.tenant))?;
            let mut changed = false;
            if tenant.placement == plan.to {
                if tenant.state != TenantState::Moving {
                    let version = tenant.version;
                    tenant.state = TenantState::Moving;
                    tenant = self
                        .store
                        .update(tenant, version)
                        .map_err(|_| self.fail(&plan.tenant))?;
                }
                tenant.placement = plan.from.clone();
                tenant.state = TenantState::Active;
                let version = tenant.version;
                self.store
                    .update(tenant, version)
                    .map_err(|_| self.fail(&plan.tenant))?;
                changed = true;
            } else if tenant.state == TenantState::Moving {
                tenant.state = TenantState::Active;
                let version = tenant.version;
                self.store
                    .update(tenant, version)
                    .map_err(|_| self.fail(&plan.tenant))?;
                changed = true;
            }
            if changed {
                self.invalidate(&plan.tenant)?;
            }
            Ok(())
        })
    }
}
