//! The ports the workflow drives, and their in-memory implementations.
//!
//! - [`SnapshotPort`]: take the production snapshot and restore it into the sandbox tenant.
//!   The host implements it over `spot-snapshot` (WP-MIG-05).
//! - [`TenantPort`]: provision a **closed** tenant and open it only at activation. The host
//!   implements it over `spot-tenancy` (WP-CTL-01).
//! - [`EffectPort`]: install the permanent outbound lock: the side-effect hold in discard mode
//!   (`spot-effect-hold`) plus a deny-all egress policy (`spot-egress`) for the sandbox tenant.
//!
//! The in-memory implementations make the whole workflow testable with no database and no
//! network. They record what they were asked to do so a test can prove the tenant never became
//! reachable before activation.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use spot_effect_hold::class::ClassPolicyConfig;
use spot_effect_hold::{EffectClass, OnDiscard, PolicyConfig, PolicyTable};
use spot_egress::{DenyEntry, EgressPolicy, Purpose, TenantPolicy};
use spot_tenancy::TenantId;

use crate::data::Document;
use crate::data::{
    Attachment, AuditRow, CacheEntry, Integration, JournalRow, SandboxData, SandboxStore,
    SearchRow, SecretRow,
};
use crate::error::{Error, Result};
use crate::plan::SnapshotRef;

/// Takes and restores snapshots. The host's implementation wraps the snapshot provider.
pub trait SnapshotPort {
    /// Snapshots `source_tenant` now (`at` is Unix microseconds). The returned reference must
    /// name the same tenant.
    ///
    /// # Errors
    /// [`Error::Snapshot`] when the provider fails.
    fn take(&self, source_tenant: &str, at: i64) -> Result<SnapshotRef>;

    /// Restores `snapshot` into `sandbox_tenant`. The tenant stays closed; restoring does not
    /// make it reachable.
    ///
    /// # Errors
    /// [`Error::Restore`] when the provider fails.
    fn restore(&self, snapshot: &SnapshotRef, sandbox_tenant: &str, at: i64) -> Result<()>;
}

/// The lifecycle status of a sandbox tenant in the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TenantStatus {
    /// Being provisioned; not usable.
    #[default]
    Provisioning,
    /// Promoted to ready (only after a clean scan).
    Ready,
    /// Failed or partly masked; must not be opened.
    Unusable,
    /// Destroyed (best effort); record retained for the administrator.
    Destroyed,
}

/// Provisions tenants and decides whether they are reachable.
pub trait TenantPort {
    /// Provisions a closed tenant named `name`, with caller entropy for its UUIDv7.
    ///
    /// # Errors
    /// [`Error::Tenant`] when the control plane refuses.
    fn provision(&self, name: &str, now_ms: u64, entropy: [u8; 10]) -> Result<TenantId>;

    /// Opens or closes a tenant. Opening before activation is exactly the failure this crate
    /// exists to prevent, so only [`crate::Sandbox::activate`] asks for `true`.
    ///
    /// # Errors
    /// [`Error::Tenant`] when the control plane refuses.
    fn set_reachable(&self, tenant: &TenantId, reachable: bool) -> Result<()>;

    /// Whether the tenant currently admits users.
    fn is_reachable(&self, tenant: &TenantId) -> bool;

    /// The tenant's lifecycle status, or `None` when the control plane does not know it.
    /// Required: an implementation that cannot report a status cannot be trusted to refuse an
    /// unusable tenant.
    fn status(&self, tenant: &TenantId) -> Option<TenantStatus>;

    /// Marks the tenant unusable (failed or partly masked) and closes it. Idempotent, and
    /// never undone: an unusable tenant is not promoted again.
    ///
    /// # Errors
    /// [`Error::Tenant`] when the control plane cannot record it.
    fn mark_unusable(&self, tenant: &TenantId) -> Result<()>;

    /// Promotes a provisioning tenant to ready. Only call after a clean scan; an unusable or
    /// destroyed tenant is refused.
    ///
    /// # Errors
    /// [`Error::Tenant`] when the tenant is not provisioning or the control plane refuses.
    fn mark_ready(&self, tenant: &TenantId) -> Result<()>;

    /// Destroys the tenant (best effort). A failure leaves it unusable and visible; it is
    /// never reported as destroyed unless the destruction happened.
    ///
    /// # Errors
    /// [`Error::Tenant`] when the tenant could not be destroyed.
    fn destroy(&self, tenant: &TenantId) -> Result<()>;
}

/// Hands out the store of one sandbox tenant. The workflow, not the caller, asks for it, so a
/// scan can only read the store of the tenant it was meant for.
pub trait StorePort {
    /// The store handle of the tenant `name` (control-plane id `tenant`). The handle's
    /// [`SandboxStore::tenant_id`] must be `name`; the workflow refuses a handle that is
    /// unbound or bound to another tenant.
    ///
    /// # Errors
    /// [`Error::Store`] when the store cannot be opened.
    fn open(&self, tenant: &TenantId, name: &str) -> Result<Box<dyn SandboxStore + '_>>;
}

/// Installs the sandbox's outbound lock.
pub trait EffectPort {
    /// Locks every outbound effect off, permanently.
    ///
    /// # Errors
    /// [`Error::Effects`] when the lock cannot be built.
    fn lock(&self, sandbox_tenant: &str, source_tenant: &str) -> Result<OutboundLock>;
}

/// The sandbox's outbound posture: discard-mode hold plus deny-all egress.
#[derive(Clone, Debug)]
pub struct OutboundLock {
    /// The sandbox tenant the policies are for.
    pub sandbox_tenant: String,
    /// The production tenant the data came from (for the record; nothing is sent to it).
    pub source_tenant: String,
    /// The tenant's egress policy: no allow entry, an explicit deny-all entry.
    pub egress: EgressPolicy,
    /// The side-effect hold configuration: every class held and dropped on discard.
    pub hold: PolicyConfig,
    /// The plain-language reason recorded with the hold.
    pub reason: String,
}

/// The classes the lock explicitly configures. Unknown classes are held by
/// [`PolicyTable`]'s default, so this list cannot open a door by omission.
pub const LOCKED_CLASSES: [&str; 7] = [
    "mail",
    "bank_api",
    "webhook",
    "connector",
    "sms",
    "push",
    "edge_sync",
];

impl OutboundLock {
    /// The permanent lock for a sandbox: no egress destination is allowed and every held class
    /// is dropped rather than released.
    #[must_use]
    pub fn deny_all(sandbox_tenant: &str, source_tenant: &str) -> Self {
        let mut egress = EgressPolicy {
            default: "deny_non_public".to_owned(),
            ..EgressPolicy::default()
        };
        // An all-`None` deny entry matches every scheme, host and port.
        let deny = vec![DenyEntry {
            scheme: None,
            host: None,
            port: None,
        }];
        egress.tenants.insert(
            sandbox_tenant.to_owned(),
            TenantPolicy {
                allow: Vec::new(),
                deny,
                connect_timeout_secs: 10,
                tls_timeout_secs: 10,
                total_timeout_secs: 30,
                max_response_bytes: 0,
                max_redirects: 0,
            },
        );

        let mut hold = PolicyConfig::default();
        for key in LOCKED_CLASSES {
            hold.overrides.insert(
                key.to_owned(),
                ClassPolicyConfig {
                    hold: Some(true),
                    never_hold: false,
                    notify_on_discard: false,
                    max_hold_seconds: None,
                },
            );
        }

        Self {
            sandbox_tenant: sandbox_tenant.to_owned(),
            source_tenant: source_tenant.to_owned(),
            egress,
            hold,
            reason: "sandbox: no outbound effect may leave a masked clone".to_owned(),
        }
    }

    /// Whether a request to `host:port` over `scheme` for `purpose` is refused.
    #[must_use]
    pub fn denies(&self, purpose: Purpose, scheme: &str, host: &str, port: u16) -> bool {
        !self
            .egress
            .is_allowed(&self.sandbox_tenant, purpose, scheme, host, port)
    }

    /// Whether the egress policy refuses every destination: no allow entry exists and the
    /// explicit deny-all entry is in place.
    #[must_use]
    pub fn denies_all(&self) -> bool {
        let Some(tenant) = self.egress.tenants.get(&self.sandbox_tenant) else {
            return false;
        };
        if !tenant.allow.is_empty() {
            return false;
        }
        let deny_all = tenant
            .deny
            .iter()
            .any(|entry| entry.scheme.is_none() && entry.host.is_none() && entry.port.is_none());
        if !deny_all {
            return false;
        }
        // The deny entry must actually refuse a representative request of every purpose.
        [
            Purpose::Mail,
            Purpose::Webhook,
            Purpose::Connector,
            Purpose::Script,
            Purpose::Llm,
            Purpose::Pdf,
            Purpose::App(7),
        ]
        .into_iter()
        .all(|purpose| {
            self.denies(purpose, "https", "api-example", 443)
                && self.denies(purpose, "http", "127.0.0.1", 8080)
        })
    }

    /// Whether every non-fiscal class is held and dropped on discard, while a fiscal
    /// transmission keeps its never-hold rule (ADR-43).
    ///
    /// # Errors
    /// [`Error::Effects`] when the hold configuration is not a valid policy table or a class
    /// would be released.
    pub fn holds_forever(&self) -> Result<()> {
        let table = PolicyTable::load(self.hold.clone()).map_err(|error| Error::Effects {
            reason: error.to_string(),
        })?;
        for key in LOCKED_CLASSES {
            let class = class_for(key);
            let policy = table.policy_for(&class);
            if !policy.hold || policy.never_hold {
                return Err(Error::Effects {
                    reason: format!("class {key} would not be held"),
                });
            }
            if policy.on_discard != OnDiscard::Drop {
                return Err(Error::Effects {
                    reason: format!("class {key} would not be discarded"),
                });
            }
        }
        if !table.is_never_hold(&EffectClass::FiscalTransmission) {
            return Err(Error::Effects {
                reason: "a fiscal transmission must never be held".to_owned(),
            });
        }
        Ok(())
    }
}

fn class_for(key: &str) -> EffectClass {
    match key {
        "mail" => EffectClass::Mail,
        "bank_api" => EffectClass::BankApi,
        "webhook" => EffectClass::Webhook,
        "connector" => EffectClass::Connector,
        "sms" => EffectClass::Sms,
        "push" => EffectClass::Push,
        "edge_sync" => EffectClass::EdgeSync,
        other => EffectClass::Other(other.to_owned()),
    }
}

/// An in-memory snapshot provider. The snapshot it takes is a record; nothing is copied.
#[derive(Debug, Default)]
pub struct MemorySnapshots {
    taken: RefCell<Option<SnapshotRef>>,
    restores: RefCell<Vec<(String, String)>>,
    fail_take: RefCell<Option<String>>,
    fail_restore: RefCell<Option<String>>,
}

impl MemorySnapshots {
    /// A provider that succeeds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next `take` fail with `reason`.
    #[must_use]
    pub fn failing_take(self, reason: impl Into<String>) -> Self {
        *self.fail_take.borrow_mut() = Some(reason.into());
        self
    }

    /// Makes the next `restore` fail with `reason`.
    #[must_use]
    pub fn failing_restore(self, reason: impl Into<String>) -> Self {
        *self.fail_restore.borrow_mut() = Some(reason.into());
        self
    }

    /// The snapshot taken, if any.
    #[must_use]
    pub fn taken(&self) -> Option<SnapshotRef> {
        self.taken.borrow().clone()
    }

    /// `(snapshot id, sandbox tenant)` for every restore.
    #[must_use]
    pub fn restores(&self) -> Vec<(String, String)> {
        self.restores.borrow().clone()
    }
}

impl SnapshotPort for MemorySnapshots {
    fn take(&self, source_tenant: &str, at: i64) -> Result<SnapshotRef> {
        if let Some(reason) = self.fail_take.borrow().clone() {
            return Err(Error::Snapshot { reason });
        }
        let snapshot =
            SnapshotRef::new(format!("snap-{at}"), "memory", source_tenant, at, 1_048_576);
        *self.taken.borrow_mut() = Some(snapshot.clone());
        Ok(snapshot)
    }

    fn restore(&self, snapshot: &SnapshotRef, sandbox_tenant: &str, _at: i64) -> Result<()> {
        if let Some(reason) = self.fail_restore.borrow().clone() {
            return Err(Error::Restore { reason });
        }
        self.restores
            .borrow_mut()
            .push((snapshot.id.clone(), sandbox_tenant.to_owned()));
        Ok(())
    }
}

/// One tenant in the in-memory control plane.
#[derive(Debug)]
pub struct TenantRecord {
    /// The tenant id.
    pub id: TenantId,
    /// Whether it admits users.
    pub reachable: bool,
    /// The lifecycle status.
    pub status: TenantStatus,
}

/// An in-memory control plane: provisions UUIDv7 tenants, all closed.
#[derive(Debug, Default)]
pub struct MemoryTenants {
    tenants: RefCell<BTreeMap<String, TenantRecord>>,
    fail_mark_unusable: Cell<bool>,
    fail_destroy: Cell<bool>,
}

impl MemoryTenants {
    /// An empty control plane.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes every `mark_unusable` fail, to test a control plane that cannot quarantine.
    #[must_use]
    pub fn failing_mark_unusable(self) -> Self {
        self.fail_mark_unusable.set(true);
        self
    }

    /// Makes every `destroy` fail, to test a tenant that cannot be removed.
    #[must_use]
    pub fn failing_destroy(self) -> Self {
        self.fail_destroy.set(true);
        self
    }

    /// The record for `name`, if provisioned.
    #[must_use]
    pub fn record(&self, name: &str) -> Option<TenantRecord> {
        self.tenants.borrow().get(name).map(|record| TenantRecord {
            id: record.id,
            reachable: record.reachable,
            status: record.status,
        })
    }

    /// Every provisioned tenant name, closed or open.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.tenants.borrow().keys().cloned().collect()
    }
}

impl TenantPort for MemoryTenants {
    fn provision(&self, name: &str, now_ms: u64, entropy: [u8; 10]) -> Result<TenantId> {
        let id = TenantId::new_v7(now_ms, entropy);
        self.tenants.borrow_mut().insert(
            name.to_owned(),
            TenantRecord {
                id,
                reachable: false,
                status: TenantStatus::Provisioning,
            },
        );
        Ok(id)
    }

    fn set_reachable(&self, tenant: &TenantId, reachable: bool) -> Result<()> {
        let mut tenants = self.tenants.borrow_mut();
        let Some(record) = tenants.values_mut().find(|record| record.id == *tenant) else {
            return Err(Error::Tenant {
                reason: "the tenant is not provisioned".to_owned(),
            });
        };
        // Only a tenant promoted to ready may be opened; an unusable one never.
        if reachable && record.status != TenantStatus::Ready {
            return Err(Error::Tenant {
                reason: "the tenant is not ready and cannot be opened".to_owned(),
            });
        }
        record.reachable = reachable;
        Ok(())
    }

    fn is_reachable(&self, tenant: &TenantId) -> bool {
        self.tenants
            .borrow()
            .values()
            .any(|record| record.id == *tenant && record.reachable)
    }

    fn status(&self, tenant: &TenantId) -> Option<TenantStatus> {
        self.tenants
            .borrow()
            .values()
            .find(|record| record.id == *tenant)
            .map(|record| record.status)
    }

    fn mark_unusable(&self, tenant: &TenantId) -> Result<()> {
        if self.fail_mark_unusable.get() {
            return Err(Error::Tenant {
                reason: "the control plane could not record the tenant as unusable".to_owned(),
            });
        }
        let mut tenants = self.tenants.borrow_mut();
        let Some(record) = tenants.values_mut().find(|record| record.id == *tenant) else {
            return Err(Error::Tenant {
                reason: "the tenant is not provisioned".to_owned(),
            });
        };
        // A destroyed tenant stays destroyed; every other status becomes unusable.
        if record.status != TenantStatus::Destroyed {
            record.status = TenantStatus::Unusable;
        }
        record.reachable = false;
        Ok(())
    }

    fn mark_ready(&self, tenant: &TenantId) -> Result<()> {
        let mut tenants = self.tenants.borrow_mut();
        let Some(record) = tenants.values_mut().find(|record| record.id == *tenant) else {
            return Err(Error::Tenant {
                reason: "the tenant is not provisioned".to_owned(),
            });
        };
        if record.status != TenantStatus::Provisioning {
            return Err(Error::Tenant {
                reason: "only a provisioning tenant can be promoted to ready".to_owned(),
            });
        }
        record.status = TenantStatus::Ready;
        Ok(())
    }

    fn destroy(&self, tenant: &TenantId) -> Result<()> {
        let mut tenants = self.tenants.borrow_mut();
        let Some(record) = tenants.values_mut().find(|record| record.id == *tenant) else {
            return Err(Error::Tenant {
                reason: "the tenant is not provisioned".to_owned(),
            });
        };
        // A tenant is destroyed only after it was marked unusable, and a failed destruction
        // leaves it unusable and visible.
        if record.status != TenantStatus::Unusable && record.status != TenantStatus::Destroyed {
            return Err(Error::Tenant {
                reason: "a tenant is destroyed only after it is marked unusable".to_owned(),
            });
        }
        if self.fail_destroy.get() {
            return Err(Error::Tenant {
                reason: "the tenant could not be destroyed".to_owned(),
            });
        }
        record.status = TenantStatus::Destroyed;
        record.reachable = false;
        Ok(())
    }
}

/// An in-memory store port: one [`SandboxData`] per tenant name.
#[derive(Debug, Default)]
pub struct MemoryStores {
    stores: RefCell<BTreeMap<String, SandboxData>>,
}

impl MemoryStores {
    /// A port with no store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts the data of the tenant `name`, binding it to that tenant, replacing any earlier one.
    pub fn insert(&self, name: &str, mut data: SandboxData) {
        data.tenant = name.to_owned();
        self.stores.borrow_mut().insert(name.to_owned(), data);
    }

    /// A copy of the data of `name`, if any.
    #[must_use]
    pub fn data(&self, name: &str) -> Option<SandboxData> {
        self.stores.borrow().get(name).cloned()
    }
}

impl StorePort for MemoryStores {
    fn open(&self, _tenant: &TenantId, name: &str) -> Result<Box<dyn SandboxStore + '_>> {
        if !self.stores.borrow().contains_key(name) {
            return Err(Error::Store {
                reason: "the tenant has no store".to_owned(),
            });
        }
        Ok(Box::new(MemoryStoreHandle {
            stores: &self.stores,
            name: name.to_owned(),
        }))
    }
}

/// A handle to one tenant's data in a [`MemoryStores`], bound to that tenant's name.
struct MemoryStoreHandle<'a> {
    stores: &'a RefCell<BTreeMap<String, SandboxData>>,
    name: String,
}

impl MemoryStoreHandle<'_> {
    fn read<T>(&self, f: impl FnOnce(&SandboxData) -> Vec<T>) -> Vec<T> {
        self.stores
            .borrow()
            .get(&self.name)
            .map(f)
            .unwrap_or_default()
    }

    fn write(&self, f: impl FnOnce(&mut SandboxData)) -> Result<()> {
        let mut stores = self.stores.borrow_mut();
        let Some(data) = stores.get_mut(&self.name) else {
            return Err(Error::Store {
                reason: "the tenant has no store".to_owned(),
            });
        };
        f(data);
        Ok(())
    }
}

impl SandboxStore for MemoryStoreHandle<'_> {
    fn documents(&self) -> Vec<Document> {
        self.read(|d| d.documents.clone())
    }
    fn replace_documents(&mut self, documents: Vec<Document>) -> Result<()> {
        self.write(|d| d.documents = documents)
    }
    fn journal(&self) -> Vec<JournalRow> {
        self.read(|d| d.journal.clone())
    }
    fn replace_journal(&mut self, rows: Vec<JournalRow>) -> Result<()> {
        self.write(|d| d.journal = rows)
    }
    fn audit(&self) -> Vec<AuditRow> {
        self.read(|d| d.audit.clone())
    }
    fn replace_audit(&mut self, rows: Vec<AuditRow>) -> Result<()> {
        self.write(|d| d.audit = rows)
    }
    fn search(&self) -> Vec<SearchRow> {
        self.read(|d| d.search.clone())
    }
    fn replace_search(&mut self, rows: Vec<SearchRow>) -> Result<()> {
        self.write(|d| d.search = rows)
    }
    fn attachments(&self) -> Vec<Attachment> {
        self.read(|d| d.attachments.clone())
    }
    fn replace_attachments(&mut self, rows: Vec<Attachment>) -> Result<()> {
        self.write(|d| d.attachments = rows)
    }
    fn caches(&self) -> Vec<CacheEntry> {
        self.read(|d| d.caches.clone())
    }
    fn replace_caches(&mut self, rows: Vec<CacheEntry>) -> Result<()> {
        self.write(|d| d.caches = rows)
    }
    fn secrets(&self) -> Vec<SecretRow> {
        self.read(|d| d.secrets.clone())
    }
    fn replace_secrets(&mut self, rows: Vec<SecretRow>) -> Result<()> {
        self.write(|d| d.secrets = rows)
    }
    fn integrations(&self) -> Vec<Integration> {
        self.read(|d| d.integrations.clone())
    }
    fn replace_integrations(&mut self, rows: Vec<Integration>) -> Result<()> {
        self.write(|d| d.integrations = rows)
    }
    fn tenant_id(&self) -> String {
        self.name.clone()
    }
}

/// An in-memory effect port: records every lock it installs.
#[derive(Debug, Default)]
pub struct MemoryEffects {
    locks: RefCell<Vec<OutboundLock>>,
}

impl MemoryEffects {
    /// An effect port with no lock installed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every lock installed, in order.
    #[must_use]
    pub fn locks(&self) -> Vec<OutboundLock> {
        self.locks.borrow().clone()
    }
}

impl EffectPort for MemoryEffects {
    fn lock(&self, sandbox_tenant: &str, source_tenant: &str) -> Result<OutboundLock> {
        let lock = OutboundLock::deny_all(sandbox_tenant, source_tenant);
        lock.holds_forever()?;
        if !lock.denies_all() {
            return Err(Error::Effects {
                reason: "the egress policy still allows a destination".to_owned(),
            });
        }
        self.locks.borrow_mut().push(lock.clone());
        Ok(lock)
    }
}
