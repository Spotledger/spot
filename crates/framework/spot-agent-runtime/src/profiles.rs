//! Runtime profiles: named bundles of settings applied as one diff.
//!
//! `standalone`, `cluster-basic` and `cluster-redis` are the ADR's three shapes. A profile is
//! not a mode flag: it is a named bundle of ordinary settings keys, written as a
//! [`ChangeSet`](spot_settings::ChangeSet) through the shared store so the console can show
//! the diff, validate every value and roll the batch back when one is refused. Nothing here
//! writes a configuration file.

use spot_settings::{
    AppliedChange, Change, ChangeSet, ParamValue, SettingsRegistry, ValueStore, apply_changes,
};

use crate::error::{RuntimeError, RuntimeResult};
use crate::roles::{NodeRole, RoleSet, ServiceRestart};
use crate::settings::{
    CACHE_BACKEND_KEY, JOBS_WORKER_CONCURRENCY_KEY, NODE_PROFILE_KEY, NODE_ROLES_KEY,
    RUNTIME_SERVICE,
};

/// One of the ADR's named runtime profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// One node does everything: the platform's own cache and a bundled database.
    Standalone,
    /// Separate web/jobs/realtime nodes with the database-backed cache.
    ClusterBasic,
    /// Separate web/jobs/realtime nodes with Redis for cache and coordination.
    ClusterRedis,
}

impl Profile {
    /// Every profile, in the order the console lists them.
    pub const ALL: [Profile; 3] = [
        Profile::Standalone,
        Profile::ClusterBasic,
        Profile::ClusterRedis,
    ];

    /// The lower-case wire name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::ClusterBasic => "cluster-basic",
            Self::ClusterRedis => "cluster-redis",
        }
    }

    /// The profile named `name`, if any.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|profile| profile.name() == name)
    }

    /// The settings this profile sets, in key order.
    ///
    /// The roles are validated as a [`RoleSet`] before the bundle is returned from
    /// [`apply_profile`], so an ADR change that produced an impossible combination fails loudly.
    #[must_use]
    pub fn bundle(self) -> Vec<(String, ParamValue)> {
        let roles = match self {
            Self::Standalone => vec![
                NodeRole::Web,
                NodeRole::Jobs,
                NodeRole::Realtime,
                NodeRole::Database,
            ],
            Self::ClusterBasic | Self::ClusterRedis => {
                vec![NodeRole::Web, NodeRole::Jobs, NodeRole::Realtime]
            }
        };
        let concurrency = match self {
            Self::Standalone => 4,
            Self::ClusterBasic => 8,
            Self::ClusterRedis => 16,
        };
        let cache = match self {
            Self::Standalone | Self::ClusterBasic => "database",
            Self::ClusterRedis => "redis",
        };
        vec![
            (
                NODE_PROFILE_KEY.to_owned(),
                ParamValue::Text(self.name().to_owned()),
            ),
            (
                NODE_ROLES_KEY.to_owned(),
                ParamValue::List(
                    roles
                        .iter()
                        .map(|role| ParamValue::Text(role.name().to_owned()))
                        .collect(),
                ),
            ),
            (
                JOBS_WORKER_CONCURRENCY_KEY.to_owned(),
                ParamValue::Int(concurrency),
            ),
            (
                CACHE_BACKEND_KEY.to_owned(),
                ParamValue::Text(cache.to_owned()),
            ),
        ]
    }
}

/// What applying a profile changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileReport {
    /// The profile applied.
    pub profile: Profile,
    /// The settings diff that was written, with before and after values.
    pub changes: Vec<AppliedChange>,
    /// The services that must restart for the diff to take effect.
    pub restarts: Vec<ServiceRestart>,
}

/// Applies `profile` as one settings diff at `scope`.
///
/// # Errors
/// [`RuntimeError::RoleCombination`] when the profile's role bundle is invalid, and
/// [`RuntimeError::Settings`] when the diff is refused by the registry or the store.
pub fn apply_profile(
    store: &dyn ValueStore,
    registry: &SettingsRegistry,
    scope: &spot_settings::ScopeRef,
    profile: Profile,
) -> RuntimeResult<ProfileReport> {
    let bundle = profile.bundle();

    // Prove the role bundle is a valid node before writing anything.
    let mut role_names = Vec::new();
    for (key, value) in &bundle {
        if key == NODE_ROLES_KEY
            && let ParamValue::List(items) = value
        {
            for item in items {
                if let ParamValue::Text(name) = item {
                    role_names.push(name.clone());
                }
            }
        }
    }
    RoleSet::parse(&role_names)?;

    let change_set = bundle
        .into_iter()
        .fold(ChangeSet::new(), |set, (key, value)| {
            set.push_change(Change::set(scope.clone(), key, value))
        });
    let changes =
        apply_changes(&change_set, registry, store).map_err(|error| RuntimeError::Settings {
            detail: format!("{error}"),
        })?;

    let mut restarts = Vec::new();
    for change in &changes {
        if let Some(spec) = registry.get(&change.key)
            && spec.restart.needed()
        {
            restarts.push(ServiceRestart {
                service: RUNTIME_SERVICE.to_owned(),
                reason: format!("{} changed", change.key),
            });
        }
    }
    restarts.sort_by(|left, right| left.service.cmp(&right.service));
    restarts.dedup_by(|left, right| left.service == right.service);

    Ok(ProfileReport {
        profile,
        changes,
        restarts,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_settings::{MemoryStore, ScopeRef};

    use super::*;
    use crate::settings::runtime_registry;

    #[test]
    fn profiles_round_trip_through_their_names() {
        for profile in Profile::ALL {
            assert_eq!(Profile::from_name(profile.name()), Some(profile));
        }
        assert_eq!(Profile::from_name("cluster-huge"), None);
    }

    #[test]
    fn every_bundle_is_a_valid_role_set() {
        for profile in Profile::ALL {
            let bundle = profile.bundle();
            let roles = bundle
                .iter()
                .find(|(key, _)| key == NODE_ROLES_KEY)
                .map(|(_, value)| value.clone())
                .unwrap();
            let names: Vec<String> = match roles {
                ParamValue::List(items) => items
                    .into_iter()
                    .filter_map(|item| match item {
                        ParamValue::Text(name) => Some(name),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            assert!(
                RoleSet::parse(&names).is_ok(),
                "{} has a valid role bundle",
                profile.name()
            );
        }
    }

    #[test]
    fn applying_a_profile_writes_the_bundle_and_reports_restarts() {
        let registry = runtime_registry().unwrap();
        let store = MemoryStore::new();
        let scope = ScopeRef::Server("srv-1".to_owned());

        let report = apply_profile(&store, &registry, &scope, Profile::ClusterRedis).unwrap();
        assert_eq!(report.profile, Profile::ClusterRedis);
        assert_eq!(report.changes.len(), 4);
        assert!(
            report
                .restarts
                .iter()
                .any(|restart| restart.service == RUNTIME_SERVICE)
        );

        assert_eq!(
            store.get(&scope, NODE_PROFILE_KEY),
            Some(spot_settings::StoredValue::Value(ParamValue::Text(
                "cluster-redis".to_owned()
            )))
        );
        assert_eq!(
            store.get(&scope, CACHE_BACKEND_KEY),
            Some(spot_settings::StoredValue::Value(ParamValue::Text(
                "redis".to_owned()
            )))
        );
        assert_eq!(
            store.get(&scope, "runtime.jobs.worker_concurrency"),
            Some(spot_settings::StoredValue::Value(ParamValue::Int(16)))
        );
    }

    #[test]
    fn standalone_gives_the_node_the_database_role() {
        let registry = runtime_registry().unwrap();
        let store = MemoryStore::new();
        let scope = ScopeRef::Server("srv-1".to_owned());
        apply_profile(&store, &registry, &scope, Profile::Standalone).unwrap();
        let roles = crate::roles::read_roles(&store, &registry, &scope).unwrap();
        assert!(roles.contains(NodeRole::Database));
    }
}
