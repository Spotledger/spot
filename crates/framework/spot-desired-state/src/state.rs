//! The desired and observed state: layers, their inheritance, and the reports agents send.
//!
//! A [`DesiredState`] is one layer of intent for one group of servers and tenants: typed
//! settings, package versions and service states. Layers inherit broad to narrow — fleet, group,
//! server, tenant — using [`spot-settings`]' own [`Chain`] and [`resolve`], so a desired state
//! never invents a second settings schema.
//!
//! An [`ObservedState`] is where agent reports come from. The crate reads no clock and no
//! store: a caller passes the report source in, and every timestamp is an argument.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use spot_fleet_rollout::NodeId;
use spot_mgmt_ops::value_check::check_param_value;
use spot_mgmt_ops::{OpId, ParamValue, Risk};
use spot_settings::{
    Chain, EffectiveValue, MemoryStore, Scope, ScopeRef, SettingsRegistry, StoredValue, ValueStore,
    resolve,
};

use crate::error::Error;
use crate::ids::SettingKey;

/// Whether a service must be enabled at boot and whether it must be running.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceState {
    /// Whether the service is enabled at boot.
    pub enabled: bool,
    /// Whether the service is running now.
    pub running: bool,
}

impl ServiceState {
    /// A service state.
    #[must_use]
    pub const fn new(enabled: bool, running: bool) -> Self {
        Self { enabled, running }
    }
}

/// The catalogue operation ids a remediation plan is built from.
///
/// All ids are [`OpId`]s, so a malformed one cannot exist here. [`OperationSet::builtin`] uses
/// the platform's own ids; a caller that owns a different catalogue can replace any field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationSet {
    /// Set one setting, parameters `key` and `value` (`settings.set`).
    pub set_setting: OpId,
    /// Install a package, parameters `name` and `version` (`os.package.install`).
    pub install_package: OpId,
    /// Remove a package, parameter `name` (`os.package.remove`).
    pub remove_package: OpId,
    /// Start a service, parameter `name` (`os.service.start`).
    pub start_service: OpId,
    /// Stop a service, parameter `name` (`os.service.stop`).
    pub stop_service: OpId,
    /// Enable a service at boot, parameter `name` (`os.service.enable`).
    pub enable_service: OpId,
    /// Disable a service at boot, parameter `name` (`os.service.disable`).
    pub disable_service: OpId,
}

impl OperationSet {
    /// The platform's operation ids.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidOperation`] when a compiled-in id is not a catalogue id (a build-time
    /// mistake caught by the crate tests).
    pub fn builtin() -> Result<Self, Error> {
        fn op(parts: &[&str]) -> Result<OpId, Error> {
            let id = parts.join(".");
            OpId::new(&id).ok_or(Error::InvalidOperation { id })
        }
        Ok(Self {
            set_setting: op(&["settings", "set"])?,
            install_package: op(&["os", "package", "install"])?,
            remove_package: op(&["os", "package", "remove"])?,
            start_service: op(&["os", "service", "start"])?,
            stop_service: op(&["os", "service", "stop"])?,
            enable_service: op(&["os", "service", "enable"])?,
            disable_service: op(&["os", "service", "disable"])?,
        })
    }
}

/// One layer of desired state for a group of servers and tenants.
///
/// The group is always named. A fleet-wide layer uses [`DesiredState::fleet_wide`] with a
/// reporting label; a group layer uses [`DesiredState::for_group`]; a server override uses
/// [`DesiredState::for_server`]; a tenant layer uses [`DesiredState::for_tenant`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredState {
    /// The group of servers and tenants this layer belongs to.
    pub group: String,
    /// The scope this layer is set at.
    pub scope: ScopeRef,
    /// The settings this layer sets, in key order.
    pub settings: BTreeMap<SettingKey, ParamValue>,
    /// Package versions this layer requires, by package name.
    pub packages: BTreeMap<String, String>,
    /// Service states this layer requires, by service name.
    pub services: BTreeMap<String, ServiceState>,
    /// The servers this layer covers. A server-scoped layer covers at least its own server.
    pub servers: BTreeSet<NodeId>,
}

impl DesiredState {
    /// A fleet-wide layer with a reporting label.
    #[must_use]
    pub fn fleet_wide(label: impl Into<String>) -> Self {
        Self {
            group: label.into(),
            scope: ScopeRef::Fleet,
            settings: BTreeMap::new(),
            packages: BTreeMap::new(),
            services: BTreeMap::new(),
            servers: BTreeSet::new(),
        }
    }

    /// A layer for `group`.
    #[must_use]
    pub fn for_group(group: impl Into<String>) -> Self {
        let group = group.into();
        Self {
            scope: ScopeRef::Group(group.clone()),
            group,
            settings: BTreeMap::new(),
            packages: BTreeMap::new(),
            services: BTreeMap::new(),
            servers: BTreeSet::new(),
        }
    }

    /// A layer for one server in `group`.
    #[must_use]
    pub fn for_server(group: impl Into<String>, server: NodeId) -> Self {
        let mut state = Self {
            scope: ScopeRef::Server(server.as_str().to_owned()),
            group: group.into(),
            settings: BTreeMap::new(),
            packages: BTreeMap::new(),
            services: BTreeMap::new(),
            servers: BTreeSet::new(),
        };
        state.servers.insert(server);
        state
    }

    /// A layer for one tenant in `group`.
    #[must_use]
    pub fn for_tenant(group: impl Into<String>, tenant: impl Into<String>) -> Self {
        Self {
            scope: ScopeRef::Tenant(tenant.into()),
            group: group.into(),
            settings: BTreeMap::new(),
            packages: BTreeMap::new(),
            services: BTreeMap::new(),
            servers: BTreeSet::new(),
        }
    }

    /// Adds a desired setting.
    #[must_use]
    pub fn with_setting(mut self, key: SettingKey, value: ParamValue) -> Self {
        self.settings.insert(key, value);
        self
    }

    /// Adds a desired package version.
    #[must_use]
    pub fn with_package(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.packages.insert(name.into(), version.into());
        self
    }

    /// Adds a desired service state.
    #[must_use]
    pub fn with_service(mut self, name: impl Into<String>, state: ServiceState) -> Self {
        self.services.insert(name.into(), state);
        self
    }

    /// Adds a server this layer covers.
    #[must_use]
    pub fn with_server(mut self, server: NodeId) -> Self {
        self.servers.insert(server);
        self
    }

    /// Every server this layer covers explicitly, in id order.
    #[must_use]
    pub fn servers(&self) -> Vec<&NodeId> {
        self.servers.iter().collect()
    }
}

/// The validated set of desired-state layers and the schema they are expressed in.
///
/// Construction is the validation point: every setting must exist in the registry, be set at
/// a scope its spec allows, be writable, and hold a value that passes the spec's own type
/// check. After that, resolution and detection cannot fail.
#[derive(Clone, Debug)]
pub struct DesiredFleet {
    registry: SettingsRegistry,
    states: Vec<DesiredState>,
    operations: OperationSet,
}

impl DesiredFleet {
    /// Validates `states` against `registry` and builds the fleet, using the built-in
    /// operation ids.
    ///
    /// # Errors
    ///
    /// The first validation problem, each with a stable `desired_state.*` code.
    pub fn new(registry: SettingsRegistry, states: Vec<DesiredState>) -> Result<Self, Error> {
        Self::with_operations(registry, states, OperationSet::builtin()?)
    }

    /// Validates `states` against `registry` with an explicit operation set.
    ///
    /// # Errors
    ///
    /// The first validation problem, each with a stable `desired_state.*` code.
    pub fn with_operations(
        registry: SettingsRegistry,
        states: Vec<DesiredState>,
        operations: OperationSet,
    ) -> Result<Self, Error> {
        let mut layers = BTreeSet::new();
        for state in &states {
            validate_state(state, &registry)?;
            let canonical = format!("{}:{}", state.scope.scope().name(), state.scope.canonical());
            if !layers.insert(canonical.clone()) {
                return Err(Error::DuplicateLayer { layer: canonical });
            }
        }
        Ok(Self {
            registry,
            states,
            operations,
        })
    }

    /// The typed settings schema the desired state is expressed in.
    #[must_use]
    pub fn registry(&self) -> &SettingsRegistry {
        &self.registry
    }

    /// The remediation operation ids.
    #[must_use]
    pub fn operations(&self) -> &OperationSet {
        &self.operations
    }

    /// The layers, in construction order.
    #[must_use]
    pub fn states(&self) -> &[DesiredState] {
        &self.states
    }

    /// Every server any layer covers, in id order.
    #[must_use]
    pub fn servers(&self) -> Vec<NodeId> {
        let mut servers = BTreeSet::new();
        for state in &self.states {
            servers.extend(state.servers.iter().cloned());
            if let ScopeRef::Server(id) = &state.scope
                && let Ok(server) = NodeId::new(id.clone())
            {
                servers.insert(server);
            }
        }
        servers.into_iter().collect()
    }

    /// The group the server belongs to: the narrowest layer that names it, else a layer label.
    #[must_use]
    pub fn group_of(&self, server: &NodeId) -> Option<String> {
        let mut best: Option<(&DesiredState, Scope)> = None;
        for state in &self.states {
            let names_server = state.servers.contains(server)
                || matches!(&state.scope, ScopeRef::Server(id) if id == server.as_str());
            if !names_server {
                continue;
            }
            let scope = state.scope.scope();
            if best.is_none_or(|(_, current)| scope > current) {
                best = Some((state, scope));
            }
        }
        if let Some((state, _)) = best {
            return Some(state.group.clone());
        }
        self.states
            .iter()
            .find(|state| state.scope.scope() == Scope::Fleet)
            .map(|state| state.group.clone())
    }

    /// The effective desired state for `server` (and optionally one tenant it hosts).
    ///
    /// Inheritance is the settings schema's: fleet, then group, then server, then tenant. The
    /// narrowest value wins for a setting; package and service maps merge in the same order,
    /// the narrowest layer replacing an entry with the same name. Secret settings are not
    /// comparable and are left out; read-only settings are observed, never desired, so they are
    /// left out too.
    #[must_use]
    pub fn effective(&self, server: &NodeId, tenant: Option<&str>) -> EffectiveDesired {
        let (group, chain, store, applicable) = self.layers(server, tenant);
        let mut packages = BTreeMap::new();
        let mut services = BTreeMap::new();
        for state in &applicable {
            for (name, version) in &state.packages {
                packages.insert(name.clone(), version.clone());
            }
            for (name, service) in &state.services {
                services.insert(name.clone(), *service);
            }
        }

        let mut settings = BTreeMap::new();
        for spec in self.registry.specs() {
            if spec.secret || spec.read_only {
                continue;
            }
            let Ok(key) = SettingKey::new(spec.key.clone()) else {
                continue;
            };
            let Ok(resolved) = resolve(key.as_str(), &chain, &store, &self.registry) else {
                continue;
            };
            if let EffectiveValue::Value(value) = resolved.value {
                settings.insert(key, value);
            }
        }

        EffectiveDesired {
            server: server.clone(),
            group,
            settings,
            packages,
            services,
        }
    }

    /// The settings this server's desired state sets away from their default, with the scope
    /// that won and the default, using the settings schema's own diff.
    ///
    /// This is the console's "what does this server's desired state change?" view; secrets
    /// stay redacted.
    #[must_use]
    pub fn differences(
        &self,
        server: &NodeId,
        tenant: Option<&str>,
    ) -> Vec<spot_settings::Difference> {
        let (_group, chain, store, _applicable) = self.layers(server, tenant);
        spot_settings::differs(&chain, &store, &self.registry, None)
    }

    /// The scope chain and the store the applicable layers produce, broad to narrow.
    fn layers(
        &self,
        server: &NodeId,
        tenant: Option<&str>,
    ) -> (String, Chain, MemoryStore, Vec<&DesiredState>) {
        let group = self.group_of(server).unwrap_or_default();
        let chain = Chain::for_context(
            true,
            Some(&group),
            Some(server.as_str()),
            tenant,
            None,
            None,
        );
        let store = MemoryStore::new();
        let mut applicable: Vec<&DesiredState> = self
            .states
            .iter()
            .filter(|state| state_applies(state, server, &group, tenant))
            .collect();
        applicable.sort_by(|left, right| {
            left.scope
                .scope()
                .cmp(&right.scope.scope())
                .then_with(|| left.group.cmp(&right.group))
        });
        for state in &applicable {
            for (key, value) in &state.settings {
                store.put(
                    &state.scope,
                    key.as_str(),
                    StoredValue::Value(value.clone()),
                );
            }
        }
        (group, chain, store, applicable)
    }

    /// The effective desired state for a whole group of servers, in server order.
    #[must_use]
    pub fn effective_all(&self) -> Vec<EffectiveDesired> {
        self.servers()
            .iter()
            .map(|server| self.effective(server, None))
            .collect()
    }
}

/// Whether a layer applies to this server (and tenant).
fn state_applies(state: &DesiredState, server: &NodeId, group: &str, tenant: Option<&str>) -> bool {
    match &state.scope {
        ScopeRef::Fleet => true,
        ScopeRef::Group(id) => id == group && state.servers.contains(server),
        ScopeRef::Server(id) => id == server.as_str(),
        ScopeRef::Tenant(id) => tenant.is_some_and(|tenant| tenant == id),
        ScopeRef::Company(_) | ScopeRef::User(_) => false,
    }
}

/// Validates one layer against the typed settings schema.
fn validate_state(state: &DesiredState, registry: &SettingsRegistry) -> Result<(), Error> {
    if state.group.trim().is_empty() {
        return Err(Error::EmptyGroup);
    }
    match &state.scope {
        ScopeRef::Fleet | ScopeRef::Server(_) | ScopeRef::Tenant(_) => {}
        ScopeRef::Group(id) => {
            if id != &state.group {
                return Err(Error::GroupMismatch {
                    group: state.group.clone(),
                    scope: state.scope.canonical(),
                });
            }
        }
        ScopeRef::Company(_) | ScopeRef::User(_) => {
            return Err(Error::UnsupportedScope {
                scope: state.scope.canonical(),
            });
        }
    }
    for (key, value) in &state.settings {
        let Some(spec) = registry.get(key.as_str()) else {
            return Err(Error::UnknownSetting {
                key: key.to_string(),
            });
        };
        if spec.read_only {
            return Err(Error::ReadOnly {
                key: key.to_string(),
            });
        }
        if spec.secret {
            // Desired state is stored, hashed, diffed and logged: a secret never goes in it.
            return Err(Error::SecretSetting {
                key: key.to_string(),
            });
        }
        if !spec.scopes.contains(&state.scope.scope()) {
            return Err(Error::ScopeNotAllowed {
                key: key.to_string(),
                scope: state.scope.scope().name().to_owned(),
            });
        }
        if let Some(problem) = check_param_value(key.as_str(), &spec.value, value).first() {
            return Err(Error::InvalidValue {
                key: key.to_string(),
                reason: problem.to_string(),
            });
        }
    }
    Ok(())
}

/// A server's effective desired state, after inheritance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveDesired {
    /// The server.
    pub server: NodeId,
    /// The group the server belongs to.
    pub group: String,
    /// Effective settings, including defaults, in key order. Secrets and read-only settings
    /// are absent.
    pub settings: BTreeMap<SettingKey, ParamValue>,
    /// Effective package versions, by package name.
    pub packages: BTreeMap<String, String>,
    /// Effective service states, by service name.
    pub services: BTreeMap<String, ServiceState>,
}

impl EffectiveDesired {
    /// One effective setting.
    #[must_use]
    pub fn setting(&self, key: &SettingKey) -> Option<&ParamValue> {
        self.settings.get(key)
    }
}

/// Where an observed state comes from: the agent reports, one snapshot per server.
pub trait ObservedState: Send + Sync {
    /// The latest report from `server`, or `None` when the agent has not reported.
    fn snapshot(&self, server: &NodeId) -> Option<ObservedSnapshot>;

    /// When the hub last heard from `server`'s agent (unix microseconds), or `None` when it
    /// never has.
    ///
    /// There is deliberately no default: an implementation must say how fresh its reports are,
    /// so a source that cannot tell cannot be mistaken for a healthy one. Implement
    /// it with the hub's own receive time, never with a time the agent wrote into its report.
    fn last_seen(&self, server: &NodeId) -> Option<i64>;
}

/// Whether a server's reports are recent enough to compare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    /// The server reported within the deadline.
    Fresh,
    /// The server is silent: it never reported (`since` is `None`) or its last report is
    /// older than the deadline (`since` is that report's time). Also the answer when the
    /// freshness cannot be proven.
    Silent {
        /// When the server was last seen, if ever.
        since: Option<i64>,
    },
}

impl Liveness {
    /// Whether the server is silent.
    #[must_use]
    pub fn is_silent(self) -> bool {
        matches!(self, Self::Silent { .. })
    }
}

/// Classifies `server` at `now` against a silence `deadline` (both unix microseconds / a span in
/// microseconds).
///
/// Fails closed: a server with no last-seen time, a report time in the future (a clock that
/// cannot be trusted, or an agent writing its own time), or no snapshot at all is silent. A
/// report exactly `deadline` old is still fresh; one microsecond older is silent. A negative
/// deadline is treated as zero.
#[must_use]
pub fn liveness(
    observed: &dyn ObservedState,
    server: &NodeId,
    now: i64,
    deadline: i64,
) -> Liveness {
    let Some(seen) = observed.last_seen(server) else {
        return Liveness::Silent { since: None };
    };
    if observed.snapshot(server).is_none() || seen > now {
        return Liveness::Silent { since: Some(seen) };
    }
    if now.saturating_sub(seen) > deadline.max(0) {
        return Liveness::Silent { since: Some(seen) };
    }
    Liveness::Fresh
}

/// One agent's report of what a server actually runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedSnapshot {
    /// Settings the agent observed as explicitly set, in key order.
    pub settings: BTreeMap<SettingKey, ParamValue>,
    /// Installed package versions, by package name.
    pub packages: BTreeMap<String, String>,
    /// Service states, by service name.
    pub services: BTreeMap<String, ServiceState>,
    /// When the agent took the reading (unix microseconds, supplied by the caller).
    pub reported_at: i64,
}

impl ObservedSnapshot {
    /// An empty snapshot taken at `reported_at`.
    #[must_use]
    pub fn new(reported_at: i64) -> Self {
        Self {
            reported_at,
            ..Self::default()
        }
    }

    /// Adds an observed setting.
    #[must_use]
    pub fn with_setting(mut self, key: SettingKey, value: ParamValue) -> Self {
        self.settings.insert(key, value);
        self
    }

    /// Adds an observed package version.
    #[must_use]
    pub fn with_package(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.packages.insert(name.into(), version.into());
        self
    }

    /// Adds an observed service state.
    #[must_use]
    pub fn with_service(mut self, name: impl Into<String>, state: ServiceState) -> Self {
        self.services.insert(name.into(), state);
        self
    }
}

/// An in-memory [`ObservedState`], for tests and for a control plane that keeps the last report
/// per server.
#[derive(Clone, Debug, Default)]
pub struct InMemoryObserved {
    snapshots: BTreeMap<NodeId, ObservedSnapshot>,
    seen: BTreeMap<NodeId, i64>,
}

impl InMemoryObserved {
    /// No reports yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records (or replaces) the report from `server`, seen when the report says it was taken.
    ///
    /// Prefer [`InMemoryObserved::record_seen`] with the hub's receive time: `reported_at` is
    /// written by the agent.
    pub fn record(&mut self, server: NodeId, snapshot: ObservedSnapshot) {
        let seen_at = snapshot.reported_at;
        self.record_seen(server, snapshot, seen_at);
    }

    /// Records (or replaces) the report from `server`, received by the hub at `seen_at`.
    pub fn record_seen(&mut self, server: NodeId, snapshot: ObservedSnapshot, seen_at: i64) {
        self.seen.insert(server.clone(), seen_at);
        self.snapshots.insert(server, snapshot);
    }

    /// Records that the hub heard from `server` at `seen_at` without a report (a heartbeat that
    /// carries no snapshot). With no snapshot the server still reads as silent.
    pub fn record_heartbeat(&mut self, server: NodeId, seen_at: i64) {
        self.seen.insert(server, seen_at);
    }

    /// The report from `server`, if any.
    #[must_use]
    pub fn get(&self, server: &NodeId) -> Option<&ObservedSnapshot> {
        self.snapshots.get(server)
    }

    /// Forgets the report from `server` (for example when it is decommissioned).
    pub fn forget(&mut self, server: &NodeId) -> Option<ObservedSnapshot> {
        self.seen.remove(server);
        self.snapshots.remove(server)
    }

    /// How many servers reported.
    #[must_use]
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    /// Whether no server reported.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }
}

impl ObservedState for InMemoryObserved {
    fn snapshot(&self, server: &NodeId) -> Option<ObservedSnapshot> {
        self.snapshots.get(server).cloned()
    }

    fn last_seen(&self, server: &NodeId) -> Option<i64> {
        self.seen.get(server).copied()
    }
}

/// The risk of a package or service drift, which has no settings spec to take it from.
pub(crate) const PACKAGE_RISK: Risk = Risk::Medium;
/// The risk of a service drift: reversible and local to one host.
pub(crate) const SERVICE_RISK: Risk = Risk::Low;
/// The risk of a silent server: its state is unknown, so nothing may be assumed about it.
pub(crate) const SILENT_RISK: Risk = Risk::High;
