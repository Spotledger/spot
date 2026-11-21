//! The schema and seed adapters: the port semantics over a [`MigrationRunner`].
//!
//! The runner is the seam to the backend crate that owns the statements
//! (`spot-tenancy-pg`'s executor over `spot-pg-migrate`). Splitting it out keeps ADR-45's rule
//! (no hand-written SQL outside the storage crates) and lets these adapters be tested with a
//! fake runner that records what the control plane asked for and when.

use spot_tenancy::Tenant;
use spot_tenancy::model::TenantState;
use spot_tenancy::ports::{PortResult, SchemaPort, SiteSeeder};

use crate::error::AdapterError;

/// The schema work a tenant's provisioning needs, in the order the ports drive it.
///
/// Every method must be idempotent: replaying it after a crash that landed the effect is a
/// success, not a second schema or a second baseline.
pub trait MigrationRunner: Send + Sync {
    /// Creates the schema named by `tenant.placement`; an existing schema of this tenant is
    /// accepted, a foreign one is refused.
    ///
    /// # Errors
    /// [`AdapterError::Schema`] or [`AdapterError::NotConfigured`].
    fn ensure_schema(&self, tenant: &Tenant) -> Result<(), AdapterError>;

    /// Drops the schema; an absent schema is a success.
    ///
    /// # Errors
    /// [`AdapterError::Schema`] or [`AdapterError::NotConfigured`].
    fn drop_schema(&self, tenant: &Tenant) -> Result<(), AdapterError>;

    /// Applies the baseline migration (the catalogue) to the tenant schema; a second call plans
    /// only what is left, so it changes nothing.
    ///
    /// # Errors
    /// [`AdapterError::Seed`] or [`AdapterError::NotConfigured`].
    fn apply_baseline(&self, tenant: &Tenant) -> Result<(), AdapterError>;

    /// Writes the tenant's seed data; a second call writes nothing new.
    ///
    /// # Errors
    /// [`AdapterError::Seed`] or [`AdapterError::NotConfigured`].
    fn seed(&self, tenant: &Tenant) -> Result<(), AdapterError>;

    /// Removes the seed data (the schema drop removes the rest); an absent seed is a success.
    ///
    /// # Errors
    /// [`AdapterError::Seed`] or [`AdapterError::NotConfigured`].
    fn unseed(&self, tenant: &Tenant) -> Result<(), AdapterError>;
}

/// `SchemaPort`/`SiteSeeder` over a shared runner: an `&R` runs every method through `R`.
impl<R: MigrationRunner + ?Sized> MigrationRunner for &R {
    fn ensure_schema(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        (**self).ensure_schema(tenant)
    }

    fn drop_schema(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        (**self).drop_schema(tenant)
    }

    fn apply_baseline(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        (**self).apply_baseline(tenant)
    }

    fn seed(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        (**self).seed(tenant)
    }

    fn unseed(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        (**self).unseed(tenant)
    }
}

/// [`SchemaPort`](spot_tenancy::ports::SchemaPort) over a [`MigrationRunner`]: the tenant's schema
/// and its baseline migration.
pub struct SchemaAdapter<R> {
    runner: R,
}

impl<R> SchemaAdapter<R> {
    /// An adapter over `runner`.
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// The runner, for tests and for the caller that shares one executor across ports.
    #[must_use]
    pub fn runner(&self) -> &R {
        &self.runner
    }
}

impl<R: MigrationRunner> SchemaPort for SchemaAdapter<R> {
    fn create(&self, tenant: &Tenant) -> PortResult {
        self.runner
            .ensure_schema(tenant)
            .and_then(|()| self.runner.apply_baseline(tenant))
            .map_err(|error| error.failure(true))
    }

    fn drop_schema(&self, tenant: &Tenant) -> PortResult {
        // A schema is only ever dropped on the way back out of a provisioning that never
        // activated, or on the way out of a deprovisioning that already archived the data. Any
        // other state means the plane asked for something it must not have, so refuse rather
        // than destroy a live tenant's data.
        if !matches!(
            tenant.state,
            TenantState::Provisioning | TenantState::Deprovisioning
        ) {
            return Err(AdapterError::Schema.failure(false));
        }
        self.runner
            .drop_schema(tenant)
            .map_err(|error| error.failure(true))
    }
}

/// [`SiteSeeder`](spot_tenancy::ports::SiteSeeder) over a [`MigrationRunner`].
pub struct SeedAdapter<R> {
    runner: R,
}

impl<R> SeedAdapter<R> {
    /// An adapter over `runner`.
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// The runner, for tests and for the caller that shares one executor across ports.
    #[must_use]
    pub fn runner(&self) -> &R {
        &self.runner
    }
}

impl<R: MigrationRunner> SiteSeeder for SeedAdapter<R> {
    fn seed(&self, tenant: &Tenant) -> PortResult {
        self.runner
            .seed(tenant)
            .map_err(|error| error.failure(true))
    }

    fn unseed(&self, tenant: &Tenant) -> PortResult {
        self.runner
            .unseed(tenant)
            .map_err(|error| error.failure(true))
    }
}
