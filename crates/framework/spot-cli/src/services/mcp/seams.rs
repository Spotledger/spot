//! What the MCP service takes from the site, and the hooks later services plug into.
//!
//! The hooks ([`AgentDirectory`], [`KeyScopes`], [`AgentAdmission`]) are filled through the
//! [`McpHandle`] port the `mcp` module provides: `WP-AI-06.T4` installs the agent directory,
//! `WP-AI-05.T4` installs the admission, and a key-scope store installs [`KeyScopes`]. Each is read
//! on every request, so nothing is captured at start-up and a service that stops can uninstall.
//!
//! A hosting service adds a tool to `tools/list` and `tools/call` with [`McpHandle::install_tool`]
//! (retrieval, agent memory and the remedy host: contract `mcp.md`, section 5, "Extension tools")
//! and removes it with [`McpHandle::uninstall_tool`] when it stops or degrades. The tools live in
//! an [`ExtensionRegistry`] the server reads on every request, so a tool installed or removed
//! while the site runs is listed or gone on the next request, with no restart.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};

use spot_agent_tiers::Tier;
use spot_api_services::query::QueryService;
use spot_audit_chain::{AuditChain, AuditError, CheckpointPolicy, CheckpointSigner};
use spot_audit_store::PgAuditStore;
use spot_coordinator::Coordinator;
use spot_core::Clock;
use spot_iam_access::KeyScope;
use spot_jobs::{InMemoryJobStore, JobStore};
use spot_mcp::server::{MetaState, MetadataSource};
use spot_mcp::{AuditSink, ChainSink, ExtensionRegistry};
pub use spot_mcp::{ExtensionError, ExtensionSpec, ExtensionTool, InstallError};
use spot_meta::{EffectiveMeta, Registry};
use spot_permissions::PermissionEngine;
use spot_postgres::PgBackend;

use crate::live::SiteLive;
use crate::services::SiteEnv;

/// A boxed future the hooks return.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An agent's stored identity: who it is and the tier its registration grants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentity {
    /// The agent's stable id (never taken from the request).
    pub agent_id: String,
    /// The tier of the stored registration.
    pub tier: Tier,
}

/// Maps a signed-in user to the agent registration it is, if any. The tier comes from here, from
/// stored data, and never from a header or an argument.
pub trait AgentDirectory: Send + Sync {
    /// The registration of `user` in `tenant`, or `None` for an ordinary user.
    fn resolve<'a>(&'a self, tenant: &'a str, user: &'a str) -> Fut<'a, Option<AgentIdentity>>;
}

/// The stored scope of an API key. A key with no scope record acts with its owner's rights,
/// exactly as on the REST routes.
pub trait KeyScopes: Send + Sync {
    /// The scope of key `key_id` in `tenant`, if the platform recorded one.
    fn scope_of<'a>(&'a self, tenant: &'a str, key_id: &'a str) -> Fut<'a, Option<KeyScope>>;
}

/// One call asking to be admitted.
#[derive(Clone, Debug)]
pub struct AdmitCall {
    /// The tenant (site).
    pub tenant: String,
    /// The audit actor: `user:<id>` or `agent:<id>`.
    pub actor: String,
    /// Whether the caller is a registered agent.
    pub agent: bool,
}

/// What a refused admission tells the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shed {
    /// Seconds until a retry may be admitted.
    pub retry_after_secs: u64,
}

/// Held for the life of one call; dropping it releases the slot. [`Permit::finish`] also reports
/// how the call ended, for breakers that learn from outcomes.
pub struct Permit {
    #[allow(dead_code)]
    guard: Option<Box<dyn Send + Sync>>,
    finish: Option<Box<dyn FnOnce(bool) + Send + Sync>>,
}

impl Permit {
    /// A permit that holds nothing.
    #[must_use]
    pub fn none() -> Self {
        Self {
            guard: None,
            finish: None,
        }
    }

    /// A permit that keeps `guard` alive until the call ends.
    #[must_use]
    pub fn holding(guard: impl Send + Sync + 'static) -> Self {
        Self {
            guard: Some(Box::new(guard)),
            finish: None,
        }
    }

    /// Runs `report(ok)` when the call ends through [`Permit::finish`].
    #[must_use]
    pub fn on_finish(mut self, report: impl FnOnce(bool) + Send + Sync + 'static) -> Self {
        self.finish = Some(Box::new(report));
        self
    }

    /// Ends the call, telling the permit whether it succeeded, then releases it.
    pub fn finish(mut self, ok: bool) {
        if let Some(report) = self.finish.take() {
            report(ok);
        }
    }
}

/// Admission control in front of every MCP call (`WP-AI-05.T4` installs the real one).
pub trait AgentAdmission: Send + Sync {
    /// Admits the call, or sheds it.
    fn admit<'a>(&'a self, call: &'a AdmitCall) -> Fut<'a, Result<Permit, Shed>>;
}

/// One `tools/call` asking to pass the gate, after the caller is authenticated and before the
/// tool runs.
#[derive(Clone, Debug)]
pub struct GateCall {
    /// The tenant (site).
    pub tenant: String,
    /// The signed-in user's id.
    pub user: String,
    /// The agent identity, when the caller is a registered agent.
    pub agent_id: Option<String>,
    /// The agent's effective tier, when it is one.
    pub tier: Option<Tier>,
    /// The tool's name (never its arguments).
    pub tool: String,
    /// The request time, unix seconds.
    pub now_unix: i64,
}

/// Why the gate refused a call: a stable code and a sentence that quotes no value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denied {
    /// The stable code (`agent.*`, `session.*`).
    pub code: String,
    /// The sentence the caller sees.
    pub message: String,
}

/// A per-call check in front of every `tools/call` (`WP-AI-06.T4` installs the tier and support
/// session gate). A refusal is answered as a permission error and the gate records it itself.
pub trait CallGate: Send + Sync {
    /// Lets the call through, or refuses it.
    fn check<'a>(&'a self, call: &'a GateCall) -> Fut<'a, Result<(), Denied>>;
}

#[derive(Default)]
struct Slots {
    query: Option<Arc<dyn QueryService>>,
    call_gate: Option<Arc<dyn CallGate>>,
    directory: Option<Arc<dyn AgentDirectory>>,
    keys: Option<Arc<dyn KeyScopes>>,
    admission: Option<Arc<dyn AgentAdmission>>,
    admission_required: bool,
    closed: Option<String>,
    /// The tools services added; shared with every server built for this site.
    extensions: Arc<ExtensionRegistry>,
}

/// The port the `mcp` module provides: later services install their hooks here.
#[derive(Default)]
pub struct McpHandle {
    slots: RwLock<Slots>,
}

impl McpHandle {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Slots> {
        self.slots.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Slots> {
        self.slots.write().unwrap_or_else(|p| p.into_inner())
    }

    /// Installs the agent directory.
    pub fn install_directory(&self, directory: Arc<dyn AgentDirectory>) {
        self.write().directory = Some(directory);
    }

    /// Installs the query service the read tools (`query`, `search_*`) run through, in place of
    /// the main engine's (`WP-AI-05.T4` installs one over the `ai` database role's own pool).
    pub fn install_query_service(&self, query: Arc<dyn QueryService>) {
        self.write().query = Some(query);
    }

    pub(super) fn query_service(&self) -> Option<Arc<dyn QueryService>> {
        self.read().query.clone()
    }

    /// Installs the per-call gate.
    pub fn install_call_gate(&self, gate: Arc<dyn CallGate>) {
        self.write().call_gate = Some(gate);
    }

    pub(super) fn call_gate(&self) -> Option<Arc<dyn CallGate>> {
        self.read().call_gate.clone()
    }

    /// Installs the API key scope source.
    pub fn install_key_scopes(&self, keys: Arc<dyn KeyScopes>) {
        self.write().keys = Some(keys);
    }

    /// Installs the admission and, with `required`, makes `/mcp` answer 503 whenever it is
    /// absent or [`McpHandle::close`] was called (an unisolated path is never served).
    pub fn install_admission(&self, admission: Arc<dyn AgentAdmission>, required: bool) {
        let mut slots = self.write();
        slots.admission = Some(admission);
        slots.admission_required = required;
    }

    /// Declares that admission must exist before `/mcp` serves, without installing it yet.
    pub fn require_admission(&self) {
        self.write().admission_required = true;
    }

    /// Stops `/mcp` from serving (503) until [`McpHandle::reopen`], for a dependency that failed.
    pub fn close(&self, reason: &str) {
        self.write().closed = Some(reason.to_owned());
    }

    /// Lifts a [`McpHandle::close`].
    pub fn reopen(&self) {
        self.write().closed = None;
    }

    /// Installs an extension tool: it is listed for the principals whose
    /// [`ExtensionTool::spec`] is `Some` and callable from the next request.
    ///
    /// # Errors
    ///
    /// [`InstallError::BadName`] for a name that could collide with a generated tool (or is not
    /// lower snake case) and [`InstallError::Duplicate`] for a name already installed. The handle
    /// is unchanged on an error. A service that is built again on a reload uninstalls its tool
    /// first, so a duplicate means two services claim one name.
    pub fn install_tool(&self, tool: Arc<dyn ExtensionTool>) -> Result<(), InstallError> {
        self.extensions().install(tool)
    }

    /// Removes the extension tool `name`; whether one was installed. A call already running
    /// finishes with the tool it started with (the tool answers a tool error if its service has
    /// stopped); the next `tools/list` no longer shows it and a call is an unknown tool.
    pub fn uninstall_tool(&self, name: &str) -> bool {
        self.extensions().uninstall(name)
    }

    /// The registry the servers of this site read on every request.
    pub(super) fn extensions(&self) -> Arc<ExtensionRegistry> {
        Arc::clone(&self.read().extensions)
    }

    pub(super) fn directory(&self) -> Option<Arc<dyn AgentDirectory>> {
        self.read().directory.clone()
    }

    pub(super) fn keys(&self) -> Option<Arc<dyn KeyScopes>> {
        self.read().keys.clone()
    }

    /// `Err(reason)` when the endpoint must not serve; `Ok(admission)` otherwise.
    pub(super) fn gate(&self) -> Result<Option<Arc<dyn AgentAdmission>>, String> {
        let slots = self.read();
        if let Some(reason) = &slots.closed {
            return Err(reason.clone());
        }
        if slots.admission_required && slots.admission.is_none() {
            return Err("admission is required and not installed".to_owned());
        }
        Ok(slots.admission.clone())
    }
}

/// Metadata and the decision engine, read per request from the live hub, so a DocType created at
/// runtime is a tool on the next call.
pub struct LiveMeta {
    live: Arc<SiteLive>,
    empty: Arc<Registry>,
    /// The decision engine the site had when this source was built: used only when nothing is
    /// served (a state the handler never calls `state` in) or a rebuild has no good state yet.
    fallback: Arc<PermissionEngine>,
    #[allow(clippy::type_complexity)]
    cache: Mutex<Option<(Arc<EffectiveMeta>, Arc<PermissionEngine>, Arc<MetaState>)>>,
}

impl LiveMeta {
    /// A source over `live`.
    ///
    /// # Errors
    ///
    /// The reason an empty registry could not be built.
    pub fn new(live: Arc<SiteLive>, fallback: Arc<PermissionEngine>) -> Result<Self, String> {
        let empty = Registry::new(Vec::new())
            .map_err(|e| format!("the empty registry did not build ({} problems)", e.len()))?;
        Ok(Self {
            live,
            empty: Arc::new(empty),
            fallback,
            cache: Mutex::new(None),
        })
    }
}

impl MetadataSource for LiveMeta {
    fn state(&self) -> Arc<MetaState> {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        let Some(served) = self.live.served() else {
            // The handler checks for a served pair first; this is a safe answer, not a path.
            return cache.as_ref().map_or_else(
                || {
                    Arc::new(MetaState {
                        registry: Arc::clone(&self.empty),
                        engine: Arc::clone(&self.fallback),
                    })
                },
                |(_, _, state)| Arc::clone(state),
            );
        };
        if let Some((meta, engine, state)) = cache.as_ref()
            && Arc::ptr_eq(meta, &served.meta)
            && Arc::ptr_eq(engine, &served.decision)
        {
            return Arc::clone(state);
        }
        let state = Arc::new(MetaState {
            registry: Arc::new(Registry::from_effective(Arc::clone(&served.meta))),
            engine: Arc::clone(&served.decision),
        });
        *cache = Some((
            Arc::clone(&served.meta),
            Arc::clone(&served.decision),
            Arc::clone(&state),
        ));
        state
    }
}

/// The checkpoint signer of a chain that writes no checkpoints (`every = 0`): asked to sign, it
/// refuses, so an unsigned checkpoint can never be written.
struct NoSigner;

impl CheckpointSigner for NoSigner {
    fn key_id(&self) -> String {
        String::new()
    }

    fn sign(&self, _message: &[u8]) -> Result<Vec<u8>, AuditError> {
        Err(AuditError::Signer("no checkpoint key is configured".into()))
    }
}

/// The AI audit sink: the tamper-evident chain on the site's own database, scope `mcp-<site>`.
pub fn audit_sink(backend: &PgBackend, site: &str) -> Arc<dyn AuditSink> {
    Arc::new(ChainSink::new(AuditChain::new(
        PgAuditStore::new(backend.pool()),
        format!("mcp-{site}"),
        Arc::new(NoSigner),
        CheckpointPolicy { every: 0 },
    )))
}

/// Whether the site's audit tables carry every protection the chain needs.
pub async fn audit_ready(backend: &PgBackend) -> Result<(), String> {
    let protection = spot_audit_store::verify(&backend.pool())
        .await
        .map_err(|e| format!("the audit store cannot be read: {e}"))?;
    if protection.is_complete() {
        Ok(())
    } else {
        Err("the audit store is not prepared (run `spot audit-prepare`)".to_owned())
    }
}

/// The coordinator the per-principal and per-address rate limits go through: the database one on
/// the cluster profile, the in-process one otherwise (the same choice `events` makes).
pub async fn coordinator(
    env: &SiteEnv<'_>,
    clock: Arc<dyn Clock>,
    site: &str,
) -> Result<Arc<dyn Coordinator>, String> {
    if env.loaded.common.services.profile == "cluster" {
        let coordinator = spot_coordinator_pg::PgCoordinator::new(
            env.backend.pool(),
            spot_coordinator_pg::PgCoordinatorConfig::default(),
        );
        coordinator
            .prepare()
            .await
            .map_err(|e| format!("the coordinator: {}", e.code))?;
        return Ok(Arc::new(coordinator));
    }
    let suffix = spot_storage_contract::RecordId::new_v7().to_string();
    let suffix = suffix.rsplit('-').next().unwrap_or("0");
    let node = spot_coordinator::NodeId::new(format!("mcp-{site}-{suffix}"))
        .map_err(|e| format!("the node name: {}", e.code))?;
    let coordinator = spot_coordinator::InProcessCoordinator::start(
        Arc::new(spot_coordinator::InMemoryPresence::new()),
        node,
        clock,
    )
    .await
    .map_err(|e| format!("the coordinator: {}", e.code))?;
    Ok(coordinator)
}

/// The job store long operations are queued on, and whether it is the durable one the workers
/// read. On the in-memory profile the store is private to this service and nothing runs its jobs.
pub async fn job_store(env: &SiteEnv<'_>) -> Result<(Arc<dyn JobStore>, bool), String> {
    if env.loaded.common.services.jobs.store == "postgres" {
        let store = crate::services::jobs::durable_store(&env.backend);
        store
            .prepare()
            .await
            .map_err(|e| format!("the job store: {e}"))?;
        Ok((Arc::new(store), true))
    } else {
        Ok((Arc::new(InMemoryJobStore::new()), false))
    }
}
