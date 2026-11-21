//! The IAM adapter: creates the tenant admin and break-glass users.
//!
//! The platform has no user-provisioning crate yet (WP-IAM owns it), so the seam is the
//! narrowest thing that can hold the port's contract: create the users, remove them. The
//! default, [`UnconfiguredIam`], fails closed: an unconfigured control plane reports the step as
//! failed instead of pretending the users exist.

use spot_tenancy::Tenant;
use spot_tenancy::ports::{IamPort, PortResult};

use crate::error::AdapterError;

/// Creates and removes a tenant's admin and break-glass users. Every method is idempotent: a
/// second create neither duplicates nor rotates the users, and removing absent users succeeds.
pub trait UserProvisioning: Send + Sync {
    /// Creates the users; re-running changes nothing.
    ///
    /// # Errors
    /// [`AdapterError::Iam`] or [`AdapterError::NotConfigured`].
    fn create_users(&self, tenant: &Tenant) -> Result<(), AdapterError>;

    /// Removes the users and their credentials; absent users are a success.
    ///
    /// # Errors
    /// [`AdapterError::Iam`] or [`AdapterError::NotConfigured`].
    fn remove_users(&self, tenant: &Tenant) -> Result<(), AdapterError>;
}

/// An `&P` is a [`UserProvisioning`] too, so one source can be shared.
impl<P: UserProvisioning + ?Sized> UserProvisioning for &P {
    fn create_users(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        (**self).create_users(tenant)
    }

    fn remove_users(&self, tenant: &Tenant) -> Result<(), AdapterError> {
        (**self).remove_users(tenant)
    }
}

/// [`IamPort`](spot_tenancy::ports::IamPort) over a [`UserProvisioning`].
pub struct IamAdapter<P> {
    provisioning: P,
}

impl<P> IamAdapter<P> {
    /// An adapter over `provisioning`.
    pub fn new(provisioning: P) -> Self {
        Self { provisioning }
    }

    /// The provisioning source, for the caller that also drives it elsewhere.
    #[must_use]
    pub fn provisioning(&self) -> &P {
        &self.provisioning
    }
}

impl<P: UserProvisioning> IamPort for IamAdapter<P> {
    fn create_users(&self, tenant: &Tenant) -> PortResult {
        self.provisioning
            .create_users(tenant)
            .map_err(|error| error.failure(true))
    }

    fn remove_users(&self, tenant: &Tenant) -> PortResult {
        self.provisioning
            .remove_users(tenant)
            .map_err(|error| error.failure(true))
    }
}

/// The default IAM source: every call fails with [`AdapterError::NotConfigured`], so a control
/// plane wired without user provisioning fails the step and stops.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnconfiguredIam;

impl UserProvisioning for UnconfiguredIam {
    fn create_users(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        Err(AdapterError::NotConfigured)
    }

    fn remove_users(&self, _tenant: &Tenant) -> Result<(), AdapterError> {
        Err(AdapterError::NotConfigured)
    }
}
