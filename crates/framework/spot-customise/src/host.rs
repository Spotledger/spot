//! What the service asks of its host: who may customise, where the default grants go, where the
//! audit trail and the lifecycle events go, and who else refers to a field.
//!
//! Each seam is a small trait with an in-memory implementation, so the pipeline is testable
//! alone and a host wires the real services (permissions, the audit chain, the event outbox).

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard};

pub use spot_permissions::Right;

/// The platform administrator role: holds every platform right, `Customise` included.
pub const SYSTEM_MANAGER: &str = "System Manager";

/// Who is calling: a user and the roles the session holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The user's id, recorded in the audit trail.
    pub user: String,
    /// The roles the session holds.
    pub roles: BTreeSet<String>,
}

impl Caller {
    /// A caller holding `roles`.
    #[must_use]
    pub fn new(user: &str, roles: &[&str]) -> Self {
        Self {
            user: user.to_owned(),
            roles: roles.iter().map(|r| (*r).to_owned()).collect(),
        }
    }

    /// Whether the caller holds `role`.
    #[must_use]
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.contains(role)
    }
}

/// Decides who may customise.
pub trait CustomiseAccess: Send + Sync {
    /// Whether the caller holds the platform `Customise` right.
    fn may_customise(&self, caller: &Caller) -> bool;

    /// Whether the caller is a platform administrator (may pick a module other than `custom`).
    fn is_platform_admin(&self, caller: &Caller) -> bool;
}

/// Role-based access: the `Customise` right is held by the listed roles; `System Manager`
/// always holds it and is the platform administrator.
#[derive(Clone, Debug, Default)]
pub struct RoleAccess {
    customise_roles: BTreeSet<String>,
}

impl RoleAccess {
    /// Only `System Manager` may customise.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Also lets `role` customise.
    #[must_use]
    pub fn granting(mut self, role: &str) -> Self {
        self.customise_roles.insert(role.to_owned());
        self
    }
}

impl CustomiseAccess for RoleAccess {
    fn may_customise(&self, caller: &Caller) -> bool {
        caller.has_role(SYSTEM_MANAGER)
            || caller
                .roles
                .iter()
                .any(|r| self.customise_roles.contains(r))
    }

    fn is_platform_admin(&self, caller: &Caller) -> bool {
        caller.has_role(SYSTEM_MANAGER)
    }
}

/// One default grant on a new DocType.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultGrant {
    /// The DocType.
    pub doctype: String,
    /// The role.
    pub role: String,
    /// The rights.
    pub rights: Vec<Right>,
}

/// Receives the default grants of a newly created DocType.
///
/// The service passes exactly one grant — `System Manager` read, write, create, delete — so a
/// new type is usable by its creator at once and by nobody else until someone grants more
/// (default closed). Idempotent: recovery may send it again.
pub trait GrantSink: Send + Sync {
    /// Records `grants` for `doctype`, replacing any earlier default grants for it.
    ///
    /// # Errors
    ///
    /// A sentence describing why the grants could not be recorded.
    fn set_default_grants(&self, doctype: &str, grants: &[DefaultGrant]) -> Result<(), String>;
}

/// One audited change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// Who made the change.
    pub actor: String,
    /// What kind of change (`create_doctype`, `update_doctype`, `add_custom_field`,
    /// `remove_custom_field`, `set_property`, `clear_property`).
    pub action: &'static str,
    /// What it was made to: `<doctype>`, `<doctype>.<field>` or `<doctype>.<field>.<property>`.
    pub subject: String,
    /// The text before the change; `None` when there was none.
    pub before: Option<String>,
    /// The text after the change; `None` when it was removed.
    pub after: Option<String>,
    /// The metadata version the change produced.
    pub version: u64,
}

/// Receives the audit trail.
pub trait AuditSink: Send + Sync {
    /// Records one change.
    fn record(&self, entry: AuditEntry);
}

/// A lifecycle event the pipeline emits when a change is live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomiseEvent {
    /// `doctype_created`, `doctype_updated`, `field_added`, `field_removed`,
    /// `property_set` or `property_cleared`.
    pub kind: &'static str,
    /// The DocType concerned.
    pub doctype: String,
    /// The field concerned, when there is one.
    pub field: Option<String>,
    /// The metadata version the change produced.
    pub version: u64,
}

/// Receives the lifecycle events.
pub trait EventSink: Send + Sync {
    /// Emits one event.
    fn emit(&self, event: CustomiseEvent);
}

/// One thing that still refers to a field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Referrer {
    /// What kind of thing: `rule`, `view`, `permission`, `index`, `field`, ...
    pub kind: String,
    /// Its name.
    pub name: String,
}

/// Finds what, outside the metadata, refers to a field: rules, views, permissions.
pub trait ReferrerLookup: Send + Sync {
    /// Everything that refers to `doctype.field`.
    fn referrers(&self, doctype: &str, field: &str) -> Vec<Referrer>;
}

/// A host with no rules, views or permissions that name fields.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoReferrers;

impl ReferrerLookup for NoReferrers {
    fn referrers(&self, _doctype: &str, _field: &str) -> Vec<Referrer> {
        Vec::new()
    }
}

/// An in-memory host for tests and a single-process start: it records everything it receives.
#[derive(Debug, Default)]
pub struct MemoryHost {
    grants: Mutex<Vec<DefaultGrant>>,
    audit: Mutex<Vec<AuditEntry>>,
    events: Mutex<Vec<CustomiseEvent>>,
    referrers: Mutex<Vec<(String, String, Referrer)>>,
}

impl MemoryHost {
    /// An empty host.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `referrer` as referring to `doctype.field`.
    pub fn add_referrer(&self, doctype: &str, field: &str, referrer: Referrer) {
        lock(&self.referrers).push((doctype.to_owned(), field.to_owned(), referrer));
    }

    /// The default grants recorded, in order.
    #[must_use]
    pub fn grants(&self) -> Vec<DefaultGrant> {
        lock(&self.grants).clone()
    }

    /// The audit trail, in order.
    #[must_use]
    pub fn audit(&self) -> Vec<AuditEntry> {
        lock(&self.audit).clone()
    }

    /// The events emitted, in order.
    #[must_use]
    pub fn events(&self) -> Vec<CustomiseEvent> {
        lock(&self.events).clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl GrantSink for MemoryHost {
    fn set_default_grants(&self, doctype: &str, grants: &[DefaultGrant]) -> Result<(), String> {
        let mut held = lock(&self.grants);
        held.retain(|g| g.doctype != doctype);
        held.extend(grants.iter().cloned());
        Ok(())
    }
}

impl AuditSink for MemoryHost {
    fn record(&self, entry: AuditEntry) {
        lock(&self.audit).push(entry);
    }
}

impl EventSink for MemoryHost {
    fn emit(&self, event: CustomiseEvent) {
        lock(&self.events).push(event);
    }
}

impl ReferrerLookup for MemoryHost {
    fn referrers(&self, doctype: &str, field: &str) -> Vec<Referrer> {
        lock(&self.referrers)
            .iter()
            .filter(|(d, f, _)| d == doctype && f == field)
            .map(|(_, _, r)| r.clone())
            .collect()
    }
}
