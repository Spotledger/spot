//! The licence adapter: `spot-license`'s issuing side behind [`LicencePort`].
//!
//! The control plane mints one licence per tenant, signed with the issuer key the caller holds.
//! Replaying the step reuses the active envelope, so a resumed provisioning never bumps the
//! serial or rotates the grant; revoking an absent licence is a success. Only the envelope is
//! kept, never a key: the signing key lives with the caller and never crosses this boundary.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_core::Clock;
use spot_license::SignedLicence;
use spot_license::issuing::Issuer;
use spot_license::model::{Edition, Entitlements, Kind, Licence, PAYLOAD_VERSION, RawLicence};
use spot_tenancy::Tenant;
use spot_tenancy::model::TenantId;
use spot_tenancy::ports::{LicencePort, PortResult};

use crate::error::AdapterError;

/// Where issued licences are persisted. The caller owns the storage (a file, a table); the
/// adapter only needs the four operations below.
pub trait LicenceStore: Send + Sync {
    /// The envelope currently issued to `tenant`, if any.
    ///
    /// # Errors
    /// [`AdapterError::LicenceStore`].
    fn active(&self, tenant: &TenantId) -> Result<Option<SignedLicence>, AdapterError>;

    /// Records `licence` as the tenant's active envelope and `serial` as its high-water mark.
    ///
    /// # Errors
    /// [`AdapterError::LicenceStore`].
    fn save(
        &self,
        tenant: &TenantId,
        licence: &SignedLicence,
        serial: u64,
    ) -> Result<(), AdapterError>;

    /// Removes the active envelope; an absent one is a success.
    ///
    /// # Errors
    /// [`AdapterError::LicenceStore`].
    fn revoke(&self, tenant: &TenantId) -> Result<(), AdapterError>;

    /// The highest serial ever issued to `tenant` (`0` when none was), so a licence issued after
    /// a revocation still increases the serial and cannot be replayed as the revoked one.
    ///
    /// # Errors
    /// [`AdapterError::LicenceStore`].
    fn highest_serial(&self, tenant: &TenantId) -> Result<u64, AdapterError>;
}

#[derive(Default)]
struct StoreState {
    active: BTreeMap<TenantId, SignedLicence>,
    highest: BTreeMap<TenantId, u64>,
}

/// An `&S` is a [`LicenceStore`] too, so one store can be shared.
impl<S: LicenceStore + ?Sized> LicenceStore for &S {
    fn active(&self, tenant: &TenantId) -> Result<Option<SignedLicence>, AdapterError> {
        (**self).active(tenant)
    }

    fn save(
        &self,
        tenant: &TenantId,
        licence: &SignedLicence,
        serial: u64,
    ) -> Result<(), AdapterError> {
        (**self).save(tenant, licence, serial)
    }

    fn revoke(&self, tenant: &TenantId) -> Result<(), AdapterError> {
        (**self).revoke(tenant)
    }

    fn highest_serial(&self, tenant: &TenantId) -> Result<u64, AdapterError> {
        (**self).highest_serial(tenant)
    }
}

/// The in-memory [`LicenceStore`], for a single-process control plane and for tests.
#[derive(Default)]
pub struct MemoryLicenceStore {
    state: Mutex<StoreState>,
}

impl MemoryLicenceStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, StoreState>, AdapterError> {
        self.state.lock().map_err(|_| AdapterError::LicenceStore)
    }
}

impl LicenceStore for MemoryLicenceStore {
    fn active(&self, tenant: &TenantId) -> Result<Option<SignedLicence>, AdapterError> {
        Ok(self.lock()?.active.get(tenant).cloned())
    }

    fn save(
        &self,
        tenant: &TenantId,
        licence: &SignedLicence,
        serial: u64,
    ) -> Result<(), AdapterError> {
        let mut state = self.lock()?;
        state.active.insert(*tenant, licence.clone());
        let high = state.highest.entry(*tenant).or_insert(0);
        *high = (*high).max(serial);
        Ok(())
    }

    fn revoke(&self, tenant: &TenantId) -> Result<(), AdapterError> {
        self.lock()?.active.remove(tenant);
        Ok(())
    }

    fn highest_serial(&self, tenant: &TenantId) -> Result<u64, AdapterError> {
        Ok(self.lock()?.highest.get(tenant).copied().unwrap_or(0))
    }
}

/// The validity window a minted licence gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LicenceWindow {
    /// Trial, subscription or perpetual.
    pub kind: Kind,
    /// How long the licence is valid from issue, in seconds (ignored for a perpetual).
    pub validity_secs: u64,
    /// The subscription grace period after expiry, in seconds (must be zero for a trial).
    pub grace_secs: u64,
}

impl Default for LicenceWindow {
    fn default() -> Self {
        Self {
            kind: Kind::Subscription,
            validity_secs: 365 * 86_400,
            grace_secs: 7 * 86_400,
        }
    }
}

/// [`LicencePort`](spot_tenancy::ports::LicencePort) over `spot-license`.
pub struct LicenceIssuer<S> {
    issuer: Issuer,
    key_id: String,
    store: S,
    clock: Arc<dyn Clock>,
    window: LicenceWindow,
    entitlements: Entitlements,
}

impl<S: LicenceStore> LicenceIssuer<S> {
    /// An issuer over `store`, minting under `key_id` with the validity window, entitlements and
    /// injected clock the caller supplies.
    ///
    /// `issuer` must have been built with the same `key_id`; a mismatch is refused at signing.
    pub fn new(
        issuer: Issuer,
        key_id: impl Into<String>,
        store: S,
        clock: Arc<dyn Clock>,
        window: LicenceWindow,
        entitlements: Entitlements,
    ) -> Self {
        Self {
            issuer,
            key_id: key_id.into(),
            store,
            clock,
            window,
            entitlements,
        }
    }

    /// The store, for the caller that also reads the issued envelope.
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    fn mint(&self, tenant: &Tenant) -> Result<(SignedLicence, u64), AdapterError> {
        let now = self.clock.now_utc().unix_timestamp();
        if now < 0 {
            return Err(AdapterError::Licence);
        }
        let edition = Edition::new(tenant.edition.as_str()).map_err(|_| AdapterError::Licence)?;
        let expires_at = match self.window.kind {
            Kind::Perpetual => None,
            Kind::Trial | Kind::Subscription => {
                let validity =
                    i64::try_from(self.window.validity_secs).map_err(|_| AdapterError::Licence)?;
                Some(now.checked_add(validity).ok_or(AdapterError::Licence)?)
            }
        };
        let serial = self.store.highest_serial(&tenant.id)?.saturating_add(1);
        let raw = RawLicence {
            v: PAYLOAD_VERSION,
            licence_id: tenant.id.to_string(),
            tenant: Some(tenant.id.to_string()),
            installation_id: None,
            edition,
            ent: self.entitlements.clone(),
            kind: self.window.kind,
            issued_at: now,
            not_before: now,
            expires_at,
            renew_after: None,
            grace: self.window.grace_secs,
            serial,
            key_id: self.key_id.clone(),
        };
        let licence = Licence::new(raw).map_err(|_| AdapterError::Licence)?;
        let signed = self
            .issuer
            .sign(&licence)
            .map_err(|_| AdapterError::Licence)?;
        Ok((signed, serial))
    }
}

impl<S: LicenceStore> LicencePort for LicenceIssuer<S> {
    fn issue(&self, tenant: &Tenant) -> PortResult {
        // Replay: the tenant already holds a licence, so re-issuing would either rotate the grant
        // or bump a serial nobody needs. The step already landed; report success.
        if self
            .store
            .active(&tenant.id)
            .map_err(|error| error.failure(true))?
            .is_some()
        {
            return Ok(());
        }
        let (signed, serial) = self.mint(tenant).map_err(|error| error.failure(false))?;
        self.store
            .save(&tenant.id, &signed, serial)
            .map_err(|error| error.failure(true))
    }

    fn revoke(&self, tenant: &Tenant) -> PortResult {
        self.store
            .revoke(&tenant.id)
            .map_err(|error| error.failure(true))
    }
}
