//! The runtime settings this module owns and the registry it reads them through.
//!
//! Node roles and profiles are configuration, and configuration in Spotledger goes through the
//! one typed settings schema (WP-ADM-07): a key, a value type, allowed scopes, a restart hint
//! and a risk level. Nothing here writes a file directly. The roles and profiles modules build
//! a [`ChangeSet`](spot_settings::ChangeSet) against the registry below and apply it with the
//! shared [`ValueStore`](spot_settings::ValueStore), so the console sees exactly the same diff
//! it would show for any other setting.
//!
//! The registry merges the compiled-in platform seed settings with this crate's runtime keys:
//! a profile can therefore set `runtime.jobs.worker_concurrency` and a node's roles in one
//! atomic diff.

use spot_settings::{
    ParamType, ParamValue, RegistryBuilder, Restart, Risk, Scope, SettingSpec, SettingsErrorReport,
    SettingsRegistry, load_seed_catalogue,
};

/// The node's role set, stored as a list of role names.
pub const NODE_ROLES_KEY: &str = concat!("runtime", ".", "node", ".", "roles");
/// The node's selected profile name.
pub const NODE_PROFILE_KEY: &str = concat!("runtime", ".", "node", ".", "profile");
/// The cache backend a profile selects.
pub const CACHE_BACKEND_KEY: &str = concat!("runtime", ".", "cache", ".", "backend");
/// The job worker concurrency setting the profiles tune (owned by the platform seed).
pub const JOBS_WORKER_CONCURRENCY_KEY: &str =
    concat!("runtime", ".", "jobs", ".", "worker_concurrency");

/// The service whose restart applies a node role or profile change.
pub const RUNTIME_SERVICE: &str = "spot-runtime";

/// A role name is a short lower-case word, optionally hyphenated.
const ROLE_PATTERN: &str = "[a-z][a-z0-9-]{0,15}";

/// The runtime-owned setting specs.
///
/// Values are the smallest honest defaults: a standalone web node with the platform's own
/// cache, so a fresh install is valid without a profile being applied first.
#[must_use]
pub fn runtime_specs() -> Vec<SettingSpec> {
    vec![
        SettingSpec {
            key: NODE_ROLES_KEY.to_owned(),
            title: "Node Roles".to_owned(),
            summary: "The Spotledger roles this node serves.".to_owned(),
            description: Some(
                "The set of roles this node runs: web, jobs, realtime, database and branch-edge. \
                 A role change rewrites configuration and restarts the runtime service."
                    .to_owned(),
            ),
            value: ParamType::List(Box::new(ParamType::Text {
                max_len: 16,
                pattern: Some(ROLE_PATTERN.to_owned()),
            })),
            default: Some(ParamValue::List(vec![
                ParamValue::Text("web".to_owned()),
                ParamValue::Text("database".to_owned()),
            ])),
            scopes: vec![Scope::Fleet, Scope::Group, Scope::Server],
            restart: Restart::Service {
                name: RUNTIME_SERVICE.to_owned(),
            },
            risk: Risk::High,
            secret: false,
            read_only: false,
            since: "0.1.0".to_owned(),
            deprecated: None,
            tags: vec!["runtime".to_owned(), "roles".to_owned()],
            depends_on: None,
            help_url: None,
            group: Some("Runtime".to_owned()),
            order: Some(10),
        },
        SettingSpec {
            key: NODE_PROFILE_KEY.to_owned(),
            title: "Node Profile".to_owned(),
            summary: "The named runtime profile this node was configured from.".to_owned(),
            description: Some(
                "Profiles are named bundles of settings: standalone, cluster-basic and \
                 cluster-redis. Applying a profile writes the bundle as one settings diff."
                    .to_owned(),
            ),
            value: ParamType::Text {
                max_len: 32,
                pattern: Some("[a-z][a-z-]{0,31}".to_owned()),
            },
            default: Some(ParamValue::Text("standalone".to_owned())),
            scopes: vec![Scope::Fleet, Scope::Group, Scope::Server],
            restart: Restart::Service {
                name: RUNTIME_SERVICE.to_owned(),
            },
            risk: Risk::Medium,
            secret: false,
            read_only: false,
            since: "0.1.0".to_owned(),
            deprecated: None,
            tags: vec!["runtime".to_owned(), "profile".to_owned()],
            depends_on: None,
            help_url: None,
            group: Some("Runtime".to_owned()),
            order: Some(20),
        },
        SettingSpec {
            key: CACHE_BACKEND_KEY.to_owned(),
            title: "Cache Backend".to_owned(),
            summary: "Where the runtime keeps shared cache and coordination state.".to_owned(),
            description: Some(
                "The platform's own database-backed cache, or Redis for a cluster profile. \
                 The database backend needs no extra service."
                    .to_owned(),
            ),
            value: ParamType::Enum(vec!["database".to_owned(), "redis".to_owned()]),
            default: Some(ParamValue::Text("database".to_owned())),
            scopes: vec![Scope::Fleet, Scope::Group, Scope::Server],
            restart: Restart::Service {
                name: RUNTIME_SERVICE.to_owned(),
            },
            risk: Risk::Medium,
            secret: false,
            read_only: false,
            since: "0.1.0".to_owned(),
            deprecated: None,
            tags: vec!["runtime".to_owned(), "cache".to_owned()],
            depends_on: None,
            help_url: None,
            group: Some("Runtime".to_owned()),
            order: Some(30),
        },
    ]
}

/// The runtime registry: the compiled-in platform seed settings plus [`runtime_specs`].
///
/// # Errors
/// [`SettingsErrorReport`] when the compiled-in seed or these specs break a schema rule (a
/// programming mistake proven absent by the crate's tests).
pub fn runtime_registry() -> Result<SettingsRegistry, SettingsErrorReport> {
    let seed = load_seed_catalogue()?;
    RegistryBuilder::new()
        .register_all(seed)
        .register_all(runtime_specs())
        .build()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_registry_holds_the_seed_and_the_runtime_keys() {
        let registry = runtime_registry().unwrap();
        for key in [NODE_ROLES_KEY, NODE_PROFILE_KEY, CACHE_BACKEND_KEY] {
            assert!(registry.contains_key(key), "{key} is registered");
        }
        assert!(registry.contains_key("runtime.jobs.worker_concurrency"));
    }

    #[test]
    fn role_changes_declare_a_service_restart() {
        let registry = runtime_registry().unwrap();
        let spec = registry.get(NODE_ROLES_KEY).unwrap();
        assert!(spec.restart.needed());
        assert_eq!(
            spec.restart.describe().as_deref(),
            Some("restart the spot-runtime service")
        );
    }
}
