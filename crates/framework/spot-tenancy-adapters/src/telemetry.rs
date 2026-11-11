//! The telemetry adapter: registers the tenant with the platform's telemetry.
//!
//! `spot-telemetry` (WP-PLT-15) is the observability layer (logs, metrics, cost), not a tenant
//! registry, so the seam is the narrowest thing the port needs: register and deregister. The
//! default, [`UnconfiguredTelemetry`], fails closed.

use spot_tenancy::Tenant;
use spot_tenancy::ports::{PortResult, TelemetryPort};

use crate::error::AdapterError;

/// Registers and deregisters a tenant. Registering twice changes nothing; deregistering an
/// absent tenant succeeds.
pub trait TelemetryRegistry: Send + Sync {
    /// Registers the tenant.
    ///
    /// # Errors
    /// [`AdapterError::Telemetry`] or [`AdapterError::NotConfigured`].
    fn register(&self, tenant: &Tenant) -> Result<(), AdapterError>;

    /// Deregisters the tenant.
    ///
    /// # Errors
    /// [`AdapterError::Telemetry`] or [`AdapterError::NotConfigured`].
    fn deregister(&self, tenant: &Tenant) -> Result<(), AdapterError>;
}

/// An `&T` is a [`TelemetryRegistry`] too, so one registry can be shared.
impl<T: TelemetryRegistry + ?Sized> TelemetryRegistry for &T {
    fn register(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        (**self).register(tenant)
    }

    fn deregister(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        (**self).deregister(tenant)
    }
}

/// [`TelemetryPort`](spot_tenancy::ports::TelemetryPort) over a [`TelemetryRegistry`].
pub struct TelemetryAdapter<T> {
    registry: T,
}

impl<T> TelemetryAdapter<T> {
    /// An adapter over `registry`.
    pub fn new(registry: T) -> Self {
        Self { registry }
    }

    /// The registry, for the caller that also drives it elsewhere.
    #[must_use]
    pub fn registry(&self) -> &T {
        &self.registry
    }
}

impl<T: TelemetryRegistry> TelemetryPort for TelemetryAdapter<T> {
    fn register(&self, tenant: &Tenant) -> PortResult {
        self.registry
            .register(tenant)
            .map_err(|error| error.failure(true))
    }

    fn deregister(&self, tenant: &Tenant) -> PortResult {
        self.registry
            .deregister(tenant)
            .map_err(|error| error.failure(true))
    }
}

/// The default telemetry source: every call fails with [`AdapterError::NotConfigured`], so an
/// unconfigured control plane cannot report a tenant as registered when it is not.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnconfiguredTelemetry;

impl TelemetryRegistry for UnconfiguredTelemetry {
    fn register(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        Err(AdapterError::NotConfigured)
    }

    fn deregister(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        Err(AdapterError::NotConfigured)
    }
}
