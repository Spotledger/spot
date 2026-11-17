//! The key hierarchy hosted on the hub (WP-SEC-10.T5): `spot_protected_keys::Hub` over the tenant
//! root key, key release to nodes under lease, revocation with data-key rotation, and the
//! protected-group sync-class table. Guide: `docs/running-spot/protected_keys.md`.
//!
//! ```toml
//! [services.protected_keys]
//! groups = [{ name = "payroll", class = "sealed" }, { name = "prices", class = "encrypted" }]
//! ```
//!
//! # Fail closed
//!
//! A group with no mapping, an unknown class name, a duplicate group or a table over the size
//! bound refuses the site's start. Release answers only the wrapped key, only to an enrolled,
//! non-revoked node, only under the tier's lease; the node identity is the one the transport took
//! from the enrolment (an argument here), never a header. No key bytes, wrapped key or token reach
//! a log: names, epochs and counts only.
//!
//! The service is **required** when any group is `encrypted`, `sealed` or `home_only` (losing key
//! custody must stop the site) and optional otherwise.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use spot_protected_keys::{
    NodeId,
    GroupName, Hub, KeyRelease, MonoTime, PermissionTier, ProtectedKeyError, RevocationReport,
    RootKeyProvider,
};
use spot_sealed_fields::{SyncClass, SyncClassTable};
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// The module's name.
pub const NAME: &str = "protected_keys";
/// After the secrets module (1000).
pub const ORDER: u32 = 1060;
/// The most groups one tenant configures.
pub const MAX_GROUPS: usize = 1_024;

/// One configured group.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GroupSetting {
    /// The protection group.
    pub name: String,
    /// `everywhere`, `encrypted`, `sealed` or `home_only`.
    pub class: String,
}

/// `[services.protected_keys]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The groups and their sync class.
    pub groups: Vec<GroupSetting>,
}

/// The validated class table.
#[derive(Clone, Debug)]
pub struct Plan {
    /// Group to class.
    pub table: SyncClassTable,
    /// The groups, in configuration order.
    pub groups: Vec<GroupName>,
    /// Whether any group needs key custody (then the service is required).
    pub custody: bool,
}

/// Validates the settings into the class table.
///
/// # Errors
///
/// [`Error`] for an unknown class, a bad or duplicate group name, or too many groups.
pub fn plan(settings: &Settings) -> Result<Plan> {
    if settings.groups.len() > MAX_GROUPS {
        return Err(Error(format!(
            "protected_keys: at most {MAX_GROUPS} groups"
        )));
    }
    let mut table = SyncClassTable::new();
    let mut groups = Vec::new();
    let mut seen = BTreeSet::new();
    let mut custody = false;
    for row in &settings.groups {
        let class = SyncClass::parse(&row.class).ok_or_else(|| {
            Error(format!(
                "protected_keys: group {} has the unknown class `{}`",
                row.name, row.class
            ))
        })?;
        let group = GroupName::new(&row.name).map_err(|_| {
            Error(format!(
                "protected_keys: `{}` is not a usable group name",
                row.name
            ))
        })?;
        if !seen.insert(row.name.clone()) {
            return Err(Error(format!(
                "protected_keys: group {} is configured twice",
                row.name
            )));
        }
        custody |= class != SyncClass::Everywhere;
        table.set(group.clone(), class);
        groups.push(group);
    }
    Ok(Plan {
        table,
        groups,
        custody,
    })
}

/// A `RootKeyProvider` over a shared provider (the secrets service's adapter).
pub struct SharedRoot(pub Arc<dyn RootKeyProvider>);

impl RootKeyProvider for SharedRoot {
    fn tenant(&self) -> &spot_protected_keys::TenantId {
        self.0.tenant()
    }

    fn root(&self) -> spot_protected_keys::TenantRootKey {
        self.0.root()
    }

    fn wrap(
        &self,
        data_key: &spot_protected_keys::LockedKey,
        aad: &[u8],
    ) -> std::result::Result<spot_secrets::WrappedKey, ProtectedKeyError> {
        self.0.wrap(data_key, aad)
    }

    fn unwrap(
        &self,
        wrapped: &spot_secrets::WrappedKey,
        aad: &[u8],
    ) -> std::result::Result<spot_secrets::SecretBytes, ProtectedKeyError> {
        self.0.unwrap(wrapped, aad)
    }
}

/// The handle the sync hub's transport calls: `release`, `status`, `revoke`. Cheap to clone.
#[derive(Clone)]
pub struct HubKeys {
    hub: Arc<Mutex<Hub<SharedRoot>>>,
    tiers: Arc<dyn PermissionTier>,
}

impl HubKeys {
    /// A handle over a hub and the node-tier seam.
    pub fn new(hub: Hub<SharedRoot>, tiers: Arc<dyn PermissionTier>) -> Self {
        Self {
            hub: Arc::new(Mutex::new(hub)),
            tiers,
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Hub<SharedRoot>) -> R) -> R {
        f(&mut self.hub.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Releases `group`'s current key to `node` (the authenticated enrolled node) under the
    /// tier's lease. Refused for a suspended or revoked node, an unknown group, or a node with no
    /// tier (the hub's own checks).
    ///
    /// # Errors
    ///
    /// The hub's refusal.
    pub fn release(
        &self,
        node: &NodeId,
        group: &GroupName,
        now: MonoTime,
    ) -> std::result::Result<KeyRelease, ProtectedKeyError> {
        let tiers = Arc::clone(&self.tiers);
        self.with(|hub| hub.release(node, group, now, tiers.as_ref()))
    }

    /// Revokes a node: release stops at once and every group it held is rotated to a new epoch.
    /// The caller schedules the resumable re-encryption from the report.
    ///
    /// # Errors
    ///
    /// The hub's refusal (unknown or already revoked node).
    pub fn revoke(
        &self,
        node: &NodeId,
        now: MonoTime,
    ) -> std::result::Result<RevocationReport, ProtectedKeyError> {
        self.with(|hub| hub.revoke_node(node, now))
    }

    /// Creates every group of the plan that has no key yet (idempotent).
    ///
    /// # Errors
    ///
    /// The hub's refusal.
    pub fn ensure_groups(
        &self,
        groups: &[GroupName],
        now: MonoTime,
    ) -> std::result::Result<usize, ProtectedKeyError> {
        self.with(|hub| {
            let mut created = 0;
            for group in groups {
                if hub.group_epoch(group).is_none() {
                    hub.create_group(group, now)?;
                    created += 1;
                }
            }
            Ok(created)
        })
    }

    /// The number of groups the hub holds keys for.
    pub fn group_count(&self) -> usize {
        self.with(|hub| hub.groups().len())
    }
}

struct KeysService {
    level: Level,
    canary: Box<dyn Fn() -> std::result::Result<(), String> + Send + Sync>,
}

impl Service for KeysService {
    fn name(&self) -> &'static str {
        NAME
    }

    fn level(&self) -> Level {
        self.level
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move { (self.canary)() })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
}

/// Registers the module when `[services.protected_keys]` is configured; absent, it is off.
///
/// Needs the ports `dyn RootKeyProvider` (the secrets service's adapter) and `dyn PermissionTier`
/// (the permission snapshot's tiers); with groups configured and a port missing, the start is
/// refused.
///
/// # Errors
///
/// [`Error`] for invalid settings or a missing port.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let Ok(settings) = ctx.settings::<Settings>(NAME) else {
        return Ok(());
    };
    let plan = plan(&settings)?;
    if !ctx.assembling() {
        return Ok(());
    }
    let root = ctx.require_port::<dyn RootKeyProvider>()?;
    let tiers = ctx.require_port::<dyn PermissionTier>()?;
    let canary_root = Arc::clone(&root);
    let keys = HubKeys::new(Hub::new(SharedRoot(root)), tiers);
    keys.ensure_groups(&plan.groups, MonoTime::from_millis(0))
        .map_err(|e| Error(format!("protected_keys: {e}")))?;
    ctx.provide::<HubKeys>(Arc::new(keys))?;
    ctx.provide::<SyncClassTable>(Arc::new(plan.table.clone()))?;
    let level = if plan.custody {
        Level::Required
    } else {
        Level::Optional
    };
    ctx.add(Arc::new(KeysService {
        level,
        canary: Box::new(move || {
            // Wrap and unwrap a canary: the root key must work before the site serves.
            let key = spot_protected_keys::LockedKey::from_bytes(&rand::random::<[u8; 32]>())
                .map_err(|e| e.to_string())?;
            let wrapped = canary_root
                .wrap(&key, b"canary")
                .map_err(|e| e.to_string())?;
            canary_root
                .unwrap(&wrapped, b"canary")
                .map(|_| ())
                .map_err(|e| e.to_string())
        }),
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn row(name: &str, class: &str) -> GroupSetting {
        GroupSetting {
            name: name.into(),
            class: class.into(),
        }
    }

    #[test]
    fn the_class_table_is_validated_and_custody_makes_it_required() {
        let plan = plan(&Settings {
            groups: vec![row("prices", "everywhere"), row("payroll", "sealed")],
        })
        .unwrap();
        assert!(plan.custody);
        assert_eq!(plan.groups.len(), 2);
        let open = super::plan(&Settings {
            groups: vec![row("prices", "everywhere")],
        })
        .unwrap();
        assert!(!open.custody);
    }

    #[test]
    fn unknown_classes_and_duplicates_refuse_the_start() {
        assert!(
            plan(&Settings {
                groups: vec![row("a", "public")]
            })
            .is_err()
        );
        assert!(
            plan(&Settings {
                groups: vec![row("a", "sealed"), row("a", "encrypted")]
            })
            .is_err()
        );
    }
}
